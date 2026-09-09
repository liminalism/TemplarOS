# Homegarden — an activity-grown Home

*Planning document, 2026-09-07. Extrapolated from `garden.md` (a design conversation between
the user and an external reviewer, to be registered as an AKR source beside
`homefield-design-advice--1f62a430.md` and
`design-review-home-and-platform-20260901--63db09f6.md`). This is a proposal: nothing in it is
decided until it is recorded in the ledger, and §3 lists every recorded decision it would have
to revise. The authoritative form, once accepted, is the AKR records named in §12.*

---

## 0. The idea in one paragraph

Home stops being a page that contains features and becomes **a spatial model of the user's
current working set, grown from what the user actually does**. Directories appear on Home
because the user opened, launched into, edited under or returned to them; they grow larger
when activity concentrates there; they hold their place while they are used; they shrink and
finally leave when neglected; and they come back where they were if the user returns. The
user can pin, hide, grow, shrink or remove any of them, and the system treats those choices as
constraints it never overrides. Nothing is fabricated on first boot; the screen earns its
contents. The internal name is **Homegarden**: not a dashboard (instruments reporting on the
machine), not a desktop (icons on a surface), not a launcher (software you could run), but
*the places you have been tending*.

Almost every primitive this needs already exists: `TileKind::Folder { path }`, the
representation ladder, `reflow::find_resize`, `HomeField1` persistence with exact-inverse
undo, `Files.Recents1`, `Launcher1`, the shared file index's change log, and `Projects1`.
What is missing is one activity ledger, one pure scoring-and-decay model, one pure planner
that turns context records into `FieldOp`s at defined moments, and a Folder tile that shows
roles and children as it grows.

---

## 1. Where this comes from

| Input | What it contributes |
|---|---|
| `garden.md`, first half (reviewer, on a screenshot of the current Home) | The diagnosis: no visual centre of gravity, boxes around everything, widgets answering "what is the machine doing" instead of "what am I doing", a permanent Desktop listing, a permanent Session box, a Notifications box that says *All caught up*, running windows on Home, an app-icon strip, four type sizes, scarce state colour. Its proposed fix assumed a declared "current project". |
| `garden.md`, the user's response | The correction that shapes this plan: do not assume a main project; do not fabricate first-boot content; rule out performance statistics on the front page and *widgets that could be menus* (Settings named explicitly); Home needs **closer integration with the file system, not one disparate part of it**; tiled access to important parts that grow when more relevant; an organic system that notices when you opened and ran things in a folder, keeps that folder on Home in its own spot, and removes it after disuse — "a garden that gets tended". |
| `garden.md`, second half (reviewer, reworked) | The model: three object kinds (roots, growths, pinned); activity breadth over raw count; roles not application names; contexts without declared projects; mathematical decay with zero polling; spatial persistence; three visibility states; material belongs to places; a four-word vocabulary (Place, Context, Action, Group); build the activity model before any visual polish. |
| `@templaros.assessment.home-vision-review-20260905` | "The mostly-empty cold Home layout remains an unresolved product choice. Filling all space would conflict with stable placement and useful drop space." Two proposals were parked, both explicitly forbidding automatic inference. This document is the third answer to that finding, and it argues the two forbidden things were forbidden for reasons the garden respects (§3). |
| `@templaros.observation.hands-on-run-5-findings-20260904` | "(3) Home is mostly empty space, against the rule that there is to be no useless space; the largest object on it, a folder tile, drew its path in dim text and nothing else." The Folder tile is the object this plan grows; its emptiness at Large is the first thing to fix (§7). |
| `@templaros.decision.representation-ladder-is-the-organizing-principle` | "One object, one canonical route, many sizes of representation." A grown directory is a Folder tile at a higher rung — never a different UI. |
| `@templaros.decision.home-field-stable-bounded-semantic-grammar` | The constitution of the field. §3 and §6 are written against it clause by clause. |

---

## 2. What the garden philosophy asks for, distilled

Home should progressively answer four questions, in this order, and almost nothing on the
current screen answers the first three:

1. **What am I doing?** — the places with concentrated recent activity, shown large.
2. **What could I work on?** — other places still established, shown smaller.
3. **What did I recently touch?** — not a *Recent* list, but children surfacing inside the
   places they belong to.
4. **What is happening with the machine?** — the bar's indicators, not Home's real estate.

Five commitments follow, and every later section is bound by them:

- **Home deals with your stuff; menus deal with the machine.** The visible surface is mostly
  things, places and working contexts. Settings, session, and system state belong in the bar
  or in applications (§11).
- **Activity earns space.** Representation size is a consequence of relevance, not a widget
  the user resized. The reverse also holds: relevance is *the user's own actions*, never file
  churn, never build output.
- **Spatial persistence.** A place grows and shrinks where it was planted. Position changes
  only by the user's hand.
- **Decay is arithmetic, not a process.** Nothing ticks. Scores are stored with their last
  event time and evaluated when Home next needs them.
- **The machine tends the garden; the user owns it.** Pin, hide, keep, remove, grow and
  shrink are constraints the tender obeys. There is never a wizard.

---

## 3. The ledger it must live inside

The garden collides, on its face, with rules the project recorded deliberately. This section
takes each collision seriously rather than talking around it, because the rules exist for
reasons (spatial memory, honesty, no background crawling) that the garden also depends on.

### 3.1 The reframing that resolves most of it

The recorded prohibitions are on **content-driven** reflow and **proximity** semantics:

> "content changes never move tiles, global repacking never destroys spatial memory,
> proximity never creates hidden relations" — `@templaros.decision.home-field-stable-bounded-semantic-grammar`

A folder gaining files is content. A drive gaining space is content. A widget refreshing is
content. Those must never move a tile, and the garden agrees: the index change log is at most
a *medium-weight* signal (§5.1) and never repositions anything.

But **the user opening a directory, launching a terminal there, opening files under it, and
coming back tomorrow** is not content. It is a sequence of the user's own deliberate gestures
on the machine — the same class of act as a pin or a drop, which the ledger already accepts as
the legitimate cause of a layout change. The only difference is that the garden *aggregates*
those gestures instead of demanding one explicit "add to Home" per object. The user's response
in `garden.md` asks for exactly that aggregation.

So the rule this plan proposes is narrower than "activity may reflow the field":

> **A tile may change size, appear or disappear because of the user's aggregated actions on
> its path; it may never change position because of them; and it may never move, shrink or
> displace a tile the user placed or pinned.**

That keeps every property the constitution protects — spatial memory, no global repack, no
hidden relations — while admitting the one thing the constitution did not anticipate: an
object whose *presence* is earned rather than declared.

### 3.2 Clause-by-clause verdicts

| Recorded rule | Source | Verdict | How the garden honours or revises it |
|---|---|---|---|
| Content changes never move tiles | stable-bounded-semantic-grammar | **Kept** | Index changes only raise a score; no op ever follows from content alone. |
| Global repacking never destroys spatial memory | same | **Kept** | The planner emits `Resize` in place and `Place` at a remembered or nearest-free landing; it never emits `Move` (§6 rule 2). |
| Proximity never creates hidden relations | same; window groups rule 1 | **Kept** | No sibling grouping is inferred (§13). Children shown *inside* a Folder tile are containment, which is the visible relation the rule asks for. |
| Reject content-driven reflow | same | **Revised, narrowly** | "Content-driven" is defined as index/content events. User-action-driven size change is admitted under the rule in §3.1. Needs `knowledge.revise` on the decision's *consequences* paragraph. |
| Reject proximity semantics | same | **Kept** | — |
| Status updates never move tiles | home-projects-and-tool-actions | **Kept** | Status (building, failed) changes colour inside the tile, never geometry. |
| Repository discovery never runs commands or hooks | same | **Kept** | Root attribution reads directory entries (`.git` exists?) and Projects1; it never runs `git`, never crawls, never reads file contents (§5.2). |
| Never scan repositories automatically, infer projects from proximity, repack on activity | `@templaros.work.home-developer-setup-preview` | **Superseded in part** | No scan (kept). No projects inferred — a *context* is not a project (§5.7, kept). "Repack on activity" becomes "resize in place at tending moments" (revised). The setup-preview proposal can stay as an orthogonal, explicit route. |
| Neither authorizes background repository commands, automatic project inference or live repacking | home-vision-review-20260905 | **Honoured** | All three remain unauthorised: no commands, no project inference, and no *live* repacking — tending happens only at reveal (§6 rule 1). |
| Loose files are never Home tiles | home-is-momentary-and-placeable-objects-are-containers | **Kept** | The garden grows folders only. Files surface as children *inside* them. |
| The Desktop directory is a default folder/shortcut pane | same | **Kept, questioned** | It stays. §14 asks whether it should be a garden Place subject to decay instead of a permanent default. |
| Home's bar is the application launcher, Apps only | the-home-field-is-a-launcher-not-a-search | **Kept** | `garden.md`'s "search files, places, apps, commands" is not reopened. Places are tiles; the bar stays Apps. |
| The bottom bar owns ordinary automatic per-window selection | home-projects-and-tool-actions | **Kept, relied on** | Running windows leave Home (garden.md §5) — already decided; `home-taskbar-migration` acceptance is still open and becomes a dependency (§11). |
| Every capability arrives as a widget on the one screen, not a panel or tray | home-widgets, home-widget-foundations | **Tension, surfaced** | The user's response rules out "things that could be menus being widgets instead, like settings". §11 proposes moving the Session and Settings *widgets* into the bar's indicators and menu, keeping the widget *contract* for anything information-first. This needs the user's decision; it is not assumed by the rest of the plan. |
| No polling; nothing idle wakes | measurement culture (`idle-daemons-20260907.json`, `home-refresh-costs-profiled`) | **Kept** | Decay is computed on read; sources are event-driven; the index is read with bounded `ChangesSince` pages at tending moments (§10). |
| Themes own the material palette; texture may distinguish kinds | home-is-momentary… | **Extended** | Material becomes per-*place* identity (deterministic from the path), still theme-owned (§7.4). |

### 3.3 What is therefore *not* a garden feature

To be explicit about what the reframing does **not** admit:

- No tile ever changes **position** because of activity.
- No tile the user placed, moved, resized or pinned is ever resized, shrunk, moved or removed
  by the tender.
- No layout change happens while the user is looking at Home (§6 rule 1).
- No group, project or relationship is ever inferred; only presence and size are.
- No process is spawned, no repository command run, no file content read.

---

## 4. Vocabulary

`garden.md` proposes four words. They map onto what exists with one addition.

| Garden word | Meaning | Existing form | New |
|---|---|---|---|
| **Place** | A directory, drive or user-chosen location | `TileKind::Folder { path }` — already the placeable Home folder object; `HomeField1.PlaceFolder` already routes a Files window's "Add to Home" to it | A **context record** behind it (§5.4): score, last event, stage, remembered landing, observed roles, pin/hide flags |
| **Context** | A Place with enough activity to expose tools and relevant children | A Folder tile at `Representation::Wide` or `Large` | The Folder tile's Wide/Large rungs actually show roles and children (§7) |
| **Action** | Something you do to a Place: Open, Terminal, Edit, Search within | `files.open`, `project.open-terminal`, `project.open-files`, `project.open-editor`, `files.search-here`, `files.open-terminal`; providers resolve Terminal/FileManager; CodeEditor is the settled-but-unbuilt additive role | `place.open-terminal`, `place.open-editor` etc. generalised from project-only to any Place (one manifest declaration each) |
| **Group** | A spatial/semantic collection of related Places | `AppGroup` (explicit, app-on-app); `Tray` (references); `Project` (container) | **Nothing inferred.** An explicit folder-on-folder gesture *could* make a Places tray later (§13). |

Three states of visibility, from `garden.md`, map to the ladder:

| State | Meaning | Rung |
|---|---|---|
| **Growing** | Active enough to gain representation | climbs toward Large |
| **Established** | Not hot, familiar enough to stay | holds at Small or Wide |
| **Dormant** | Not displayed; identity and history retained | no tile; context record kept |

And three representation sizes, as `garden.md` recommends implementing first:

| Garden size | Rung | Span (Folder class) | Shows |
|---|---|---|---|
| trace | `Small` at the 2×2 floor | 2×2 | name, one line of state, material |
| tile | `Wide` | ≥3×2 (RungFloor 2,2,6) | name, roles row |
| expanded | `Large` | ≥3×3 (RungFloor 2,2,9), up to 8×6 | name, roles, children, state |

`Micro` is not used for Places: a 1×1 folder is an icon with no name, and `garden.md`'s
"trace" needs a name to be a trace of anything.

---

## 5. The model

### 5.1 Activity events

One normalised event, produced by several sources and consumed by one ledger:

```rust
/// One human-significant interaction with a path. Pure data; no I/O; serde.
pub struct HomeActivity {
    /// The path the event names, as native bytes (lossless, like the index).
    pub path: NativePath,
    pub kind: ActivityKind,
    /// The application that did it, when identity is known (cgroup-derived, never claimed).
    pub app_id: Option<AppId>,
    /// The role the application filled, when the launch resolved one (Terminal, FileManager,
    /// CodeEditor). What a Place learns as an "observed role".
    pub role: Option<ProviderRole>,
    pub at_ms: i64,
}

pub enum ActivityKind {
    // High weight — the user did this, on purpose, to this place.
    DirectoryOpened,        // a folder window or Files surface entered it
    FileOpened,             // brokered open, portal chooser, first-party app
    LaunchedHere,           // a launch whose working directory / --directory / argument is here
    CreatedOrRenamed,       // Files.Operations1 create/rename/move by the user
    PlacedOnHome,           // PlaceFolder / Add to Home — the strongest single signal
    ProjectEntered,         // Projects1.SetCurrent to a project rooted here
    // Medium weight — evidence of work, not a gesture.
    HumanFileModified,      // index change log: mtime on a non-ignored, human-facing file
    FileSaved,              // Recents1 "saved"/"created"/"modified" from a first-party app
    // Very low — kept only so a Downloads-style root can say "3 new".
    ArrivedHere,            // index change log: Added under a *root* directory
}
```

**Weights** (initial values; levers in `settings`, §8):

| Kind | Weight | Rationale |
|---|---|---|
| `PlacedOnHome` | 8.0 | The user explicitly asked. Also sets `pinned` (§5.4). |
| `ProjectEntered` | 5.0 | An explicit declaration of context. |
| `LaunchedHere` | 3.0 | A terminal, editor or file manager opened *at* this path. |
| `DirectoryOpened` | 2.0 | Navigation into it. |
| `FileOpened` | 1.5 | One file under it. |
| `CreatedOrRenamed` | 1.5 | A deliberate mutation. |
| `FileSaved` | 0.8 | Work, but repeated by every Ctrl+S. |
| `HumanFileModified` | 0.3, capped per bucket | An editor was here, probably. |
| `ArrivedHere` | 0.05, capped per bucket | Downloads gained a file. |

**Breadth beats count.** Raw weight is not the score. Per context and per day, the ledger
keeps *which kinds* and *which roles* were seen; the score's breadth term (§5.4) rewards a
day with a terminal, an editor and three opens far above a day with three hundred saves.

**Coalescing.** Events are bucketed by `(root, kind, hour)`. Forty saves in an hour are one
bucket with a count; a `cargo build` touching forty thousand files under `target/` is *no*
bucket because `target/` is ignored (§5.3). This is what keeps the ledger bounded and keeps a
build from becoming the centre of Home.

**Sources that exist today** and what each can feed, honestly:

| Source | Today | Feeds | Gap |
|---|---|---|---|
| Home's `Launch Tool` (`project.open-terminal` etc.) | Launches `--directory <root>` through `Launcher1` with a provider-resolved role (`surface/app.rs`) | `LaunchedHere` + role, for project roots | Only for projects; §4's `place.*` actions extend it to any Place |
| Brokered file open (`data.rs::brokered_file_open` → `Recents1.Record`) | Shell records every open it brokers | `FileOpened` with `app_id` | Only what Home/Files brokers; apps opening files themselves are invisible |
| `templar-files` backend | Records opens to `Recents1` | `FileOpened` | pcmanfm-qt is the file manager and does *not* record (`docs/architecture/files.md`: recents not integrated, writes xbel) |
| Folder windows / Files object navigation (`BrowseDirectory`) | Not recorded anywhere | `DirectoryOpened` | One call to add |
| `Projects1.SetCurrent` | Persisted, signalled | `ProjectEntered` | — |
| `Files.Operations1` journal + `History1` file-operation events | Recorded | `CreatedOrRenamed` | Needs the path, which the event carries |
| `Files.Index1.ChangesSince` | Implemented, bounded, pageable | `HumanFileModified`, `ArrivedHere` | Signals never wired (`@templaros.observation.index-change-signals-never-fire`); read on demand until they are |
| `HomeField1.PlaceFolder` / `templar home place` | Exists | `PlacedOnHome` | — |
| FileChooser portal | X1 work item, in progress | `FileOpened`, `FileSaved` for every portal-using app | Blocked on X1 |
| Terminal cwd changes after launch | Nothing | — | Deliberately none: no `ptrace`, no shell hooks. The launch directory is the signal; the ledger is honest that it is *launch*, not *cwd*. |
| pcmanfm-qt browsing | Nothing, and no plugin API | — | Custom actions ("Add to Home", "Create project here") are the outbound route; browsing itself cannot be observed. Recorded as a known blind spot, not patched around. |

The sober conclusion, matching `garden.md`'s advice: **the launch path is the strongest
signal the system has, and it already knows every launch and its arguments**. The garden
starts there and widens.

### 5.2 Root attribution

An event names a path; the ledger attributes it to a **context root**, so that work under
`Templar/renderer/` and `Templar/distro/` accumulates on `Templar`. Attribution is a walk up the
ancestors, stopping at the first that is:

1. the root of a `Projects1` project (a declared place beats every heuristic);
2. an existing context root (already in the ledger — so a context, once formed, keeps
   attracting its descendants' activity);
3. a directory containing a VCS marker (`.git`, `.hg`, `.jj`, `.svn`) or a workspace marker
   (`Cargo.toml` with `[workspace]` is *not* checked — that needs reading a file; the bare
   presence of `Cargo.toml`, `package.json`, `pyproject.toml`, `Makefile`, `justfile` at the
   highest ancestor that has one is enough);
4. a mount point (`Files.Volumes1`);
5. an XDG user directory (Downloads, Documents, …) or `$HOME`;

else the directory itself, and the walk is capped at a depth of eight above the path.

Rule 3 reads *directory entries*, never file contents and never a command; it is
`stat`-class work on at most eight ancestors per event, done once per unique path and cached
against the index generation. `@templaros.decision.home-projects-and-tool-actions` ("repository
discovery never runs commands or hooks") is kept to the letter.

A path attributed to a root also records itself as a **child candidate** of that root — the
first two path components beneath the root (`renderer/`, `distro/`, `README.md`). That list is
where §5.5's children come from.

### 5.3 The ignore list

Activity under these is dropped before attribution, because it is machine activity wearing a
path:

- hidden directories (`.git/` internals, `.cache`, `.local/state`, any `.`-prefixed component);
- `target/`, `build/`, `dist/`, `out/`, `node_modules/`, `__pycache__/`, `.venv/`, `venv/`,
  `vendor/`, `.cargo/`, `.rustup/`;
- `$XDG_CACHE_HOME`, `$XDG_STATE_HOME`, `$XDG_RUNTIME_DIR`, `/tmp`, `/var`, `/proc`,
  `/sys`, `/dev`, `/run`;
- the trash directories `Trash1` knows;
- the index's own store, sessiond's own store;
- anything the user hid (§8).

This list is a settings value (`shell.home-garden.ignore`) with these defaults, so a person
who keeps sources under `build/` can say so.

### 5.4 Score, decay, stages

Per context root, the ledger stores:

```rust
pub struct HomeContext {
    pub root: NativePath,
    /// Identity that survives a rename: the index EntryId when the root is indexed.
    pub entry: Option<EntryId>,
    /// Raw accumulated score at `last_event`. Never decayed in storage.
    pub score: f32,
    pub last_event_ms: i64,
    /// Per-day breadth: which kinds and roles were seen, for the last 14 days, as bitsets.
    pub days: [DayBreadth; 14],
    pub stage: Stage,                // Dormant | Trace | Tile | Expanded
    pub stage_since_ms: i64,
    /// Where it was last placed, and at what span, so it regrows there.
    pub landing: Option<(String /*output*/, Cell, Span)>,
    pub roles: RoleSet,              // Terminal, FileManager, CodeEditor observed here
    pub children: Vec<ChildCandidate>, // path, score, last_event — capped at 12
    pub pinned: bool,                // never decays, never removed
    pub hidden: bool,                // never grows; ledger keeps the record for the Hide list
    pub user_span: Option<Span>,     // set when the user resized it; the tender stops resizing
    pub material_seed: u64,          // deterministic from `entry` or root bytes
}
```

**Effective score** (computed on read, never stored):

```
effective(ctx, now) =
    ctx.score × exp(−ln2 × (now − ctx.last_event) / HALF_LIFE)
  × (1 + BREADTH_BONUS × breadth(ctx.days))
```

with `HALF_LIFE` = 3 days (lever `shell.home-garden.half-life`) and `breadth` = the number of
distinct `(kind, role)` pairs seen in the last 7 days, normalised to `[0, 1]`. A place used
every day for a week with mixed tools has a breadth near 1; a place that received one burst of
saves has a breadth near 0.15.

**Stage thresholds with hysteresis** — the reason a place does not flicker between sizes:

| Transition | Condition | Minimum dwell in current stage |
|---|---|---|
| Dormant → Trace | effective ≥ 3.0 (one launch, or two opens) | — |
| Trace → Tile | effective ≥ 8.0 **and** activity on ≥ 2 distinct days | 1 hour |
| Tile → Expanded | effective ≥ 20.0 **and** ≥ 2 roles or kinds **and** ≥ 3 distinct days | 6 hours |
| Expanded → Tile | effective < 10.0 | 24 hours |
| Tile → Trace | effective < 4.0 | 48 hours |
| Trace → Dormant | effective < 1.0 (about a week of silence from Trace) | 5 days |

Demotion thresholds sit well below promotion thresholds, and demotion dwell is long, so
"a context should only disappear after a fairly long period of disuse" holds arithmetically.
A pinned context never demotes. A hidden context never promotes.

**Zero polling.** Nothing evaluates this on a timer. The ledger updates one context on each
event (add weight, set `last_event`, update the day's breadth). Stages are recomputed for all
contexts only at **tending moments** (§6 rule 1). Three days without opening Home costs
nothing.

**Bounds.** At most 64 contexts are retained (`shell.home-garden.max-contexts`); beyond that,
the lowest effective non-pinned Dormant record is dropped. At most 12 child candidates per
context. The ledger is therefore a few kilobytes.

### 5.5 Children and roles

An Expanded place shows up to six **children**: the child candidates with the highest
effective score, favouring directories over files when scores tie, and always including a
pinned child. `README.md` opened twice and `renderer/` entered six times both qualify; a
file modified by a save but never opened does not, because `HumanFileModified` alone stays
below the child threshold. If activity shifts from `renderer/` to `distro/`, the list changes
at the next tending moment — *inside* the tile, which the constitution permits.

**Roles, not applications.** A place shows at most three actions: the roles it has observed
(`Terminal`, `FileManager`, `CodeEditor`), plus `FileManager` always, because every Place can
be opened. The shell resolves a role through `Providers1` at press time, exactly as
`project.open-terminal` does today; Home never hardcodes kitty or pcmanfm-qt. The
`CodeEditor` role — settled as additive in `@templaros.work.home-project-tool-actions` but not
built — is a dependency for the third action and a blank space until then.

### 5.6 Roots — the soil

`garden.md`: show a root when it is inherently important, has meaningful contents, or the user
uses it — and do not reproduce the XDG hierarchy because `xdg-user-dirs` created it.

- **Files** stays as it is: one adaptive object, the gateway into every file on the machine.
  It is not a garden Place and never decays.
- **Desktop** stays as the recorded default folder tray. §14 asks whether it should instead
  be an ordinary Place seeded pinned, so that a person who never uses it can let it recede.
- **Downloads, Documents, Pictures, …** are *not* placed by default. Downloads earns a Trace
  the first time something arrives (`ArrivedHere`) and a Tile when the user opens it; its
  Small rung says `3 new` (count of `Added` since the user last opened it). This is the one
  place content contributes a *number*; it still never moves a tile.
- **Mounted volumes** appear only as Places when used, like any directory. Capacity stays in
  Files (`Volumes1`), not on Home.
- **`$HOME` itself** is never a Place; it is the field.

A cold Home is therefore: the Apps bar, Files, Desktop, the bar below — and nothing else.
That is `garden.md`'s "not empty because the UI is unfinished; empty because the machine
hasn't learned much yet", and it resolves the unresolved finding in
`home-vision-review-20260905` by *reading* the emptiness rather than filling it.

### 5.7 Contexts and Projects

The distinction `garden.md` draws is kept exactly:

| | Working context | Project |
|---|---|---|
| Created by | inference from aggregated activity | the user (`Projects1.Create`, `project.create-here`) |
| Owns | presence and size on Home | membership, apps, environment, mounts, importance, checkpoints |
| Tile | `TileKind::Folder` | `TileKind::Project` |
| Decays | yes | never |

Two bridges, neither a wizard:

1. A project's root is *always* a context, pinned, so `Launch Tool` and `Enter Project`
   activity accrues to it and its tile shows children like any Expanded place. The Project
   tile's Large rung can draw the same roles-and-children body (`@templaros.decision.representation-ladder…`:
   one body, many objects).
2. A Folder context that has been Expanded for seven days gains one row in its context menu:
   **Make this a project** → `project.create-here` with the root already filled in. No banner,
   no notification, no first-run prompt. If the user ignores it, it stays a row.

A context is never turned into a project by the system. Ever.

---

## 6. The tender — layout rules

The **planner** is a pure function: `(contexts, document, now, output) → Vec<FieldOp>`. It
lives beside `reflow` in `templar-field` (§9) and is property-tested there. Eight rules, each with
its reason:

1. **Tending happens only at tending moments: Home reveal, session start, and the explicit
   `garden.tend` action.** Reason: the user must never see the field rearrange itself under
   their pointer, and `home-vision-review-20260905` forbids live repacking. A reveal is the
   moment the user *asks* what is current; that is when the answer may be updated. The ops
   are applied through `HomeField1.Apply` before the field is painted, so the user sees one
   settled state, and they enter the undo log like any other op.

2. **The planner never emits `Move` or `MoveMany`.** Reason: position is spatial memory. It
   emits `Place` (a context entering the field), `Resize` (a stage change), and `Remove` (a
   context going Dormant). Nothing else.

3. **A `Resize` is accepted only if `reflow::find_resize` satisfies it without relocating any
   tile and without shrinking any tile the tender does not own.** `find_resize` already
   keeps every neighbour's far edge and clamps rather than evicts; the planner adds one more
   constraint: the set of yielding neighbours may contain only garden-owned, unpinned, non-user-sized
   Folder tiles. If the clamp yields the current span, the stage is recorded but the tile
   stays its size — the ledger and the field are allowed to disagree, and the tile's state
   line says `growing · no room` so the disagreement is visible.

4. **A `Place` lands at the remembered landing if it is free, else at the nearest free cell
   to it, else at the nearest free cell to the garden's home corner.** The garden's home
   corner is the corner opposite the Files object (a setting; default bottom-left on a field
   whose Files object is top-left), so growths cluster and the user learns where to look.
   Reason: "if you return six weeks later it can regrow in approximately its old position".

5. **A tile the user has touched — moved, resized, pinned, or created via `PlaceFolder` — is
   the user's.** The tender may still change its *contents* (children, roles, state) and may
   still grow it *if* the user never resized it (a `user_span` of `None`), but it never shrinks
   or removes it. `PlaceFolder` sets `pinned`. Reason: the automatic system treats user
   choices as constraints; that is what makes it tolerable.

6. **Shrinking precedes removal, and removal takes days.** A context steps down one rung per
   tending moment at most, subject to §5.4's dwell times. Reason: the user should watch a
   place recede, not find it gone.

7. **The tender is add-only across shell versions.** A saved document that predates the
   garden is not reset; the tender grows into it. A context whose remembered landing is now
   occupied by a user tile simply lands elsewhere. Reason: `HomeField1` merge is add-only by
   design and `home-refresh` migrations show the pattern.

8. **Every op the tender emits carries a reason string, and the shell keeps the last tending
   report.** `grew Templar to 5×4: 14 opens, 3 terminals, 4 days` / `removed jp2lam: no activity
   for 9 days`. Reason: the project's culture is that interventions explain themselves
   (`resourced`'s traces, the deploy's convergence report); a Home that rearranges without
   saying why would be resented within a week. The report is one `History1` event per tending
   moment that changed something, so it appears in the history the user already has.

**Testable invariants** (added to `crates/templar-field/tests/field.rs` as proptests):

- `tend` never produces a `Move`/`MoveMany`.
- After `tend`, every tile that was user-owned has the same `Located` and `Span` as before.
- After `tend`, every tile that existed and was not removed has the same top-left cell.
- `tend` is idempotent: applying it twice at the same `now` yields no ops the second time.
- `tend(now + Δ)` for Δ inside every dwell window yields no stage changes.
- A pinned context is never in a `Remove` and never shrinks.
- A hidden context is never in a `Place`.
- The set of yielding tiles in every accepted `Resize` is garden-owned only.
- The number of ops per tending moment is bounded by the number of contexts that changed
  stage (no cascading).

---

## 7. Representation — what a Place shows

This is the Folder tile, which today draws "its path in dim text and nothing else" at Large
(`hands-on-run-5-findings`). The garden is the reason to finish it, and the ladder decision
says how: one object, four rungs, same state model.

### 7.1 Rungs

| Rung | Span | Body |
|---|---|---|
| **Small** (trace) | 2×2 | Name in the tile's own material; one state line (`4 days ago`, `growing`, `3 new`). Manipulation controls on hover/focus only (WS3). |
| **Wide** (tile) | ≥3×2 | Name; a roles row of up to three text-led actions: `>_ Terminal  ▤ Files  <> Edit`. Symbolic glyphs, not application icons — "a project should look like the project, not a collage of the applications used to manipulate it". |
| **Large** (expanded) | ≥3×3 | Name (the 20–22px project-title size); roles row; a two-column children list (up to six, directories first, each a single-click open through the same brokered route as today); state line. Optionally, at ≥5×4, a treemap band across the bottom drawn by `templar-treemap` for the root — inside the object, as the constitution requires. |
| **Full** | zoomed | The existing folder window over the same root, with the Files surface state (`location, selection, sort`) preserved, as the ladder decision demands. |

Children in the Large body are references: pressing one opens it; dragging one out of the tile
onto the field is the existing `route_file_drop` (into the Shortcuts pane), never a loose tile.

### 7.2 Typography

`garden.md`'s four levels, applied to Places only (the rest of Home keeps its `design.rs`
scale):

| Level | Size | Use |
|---|---|---|
| 1 | 28–32 px | The name of an Expanded place at ≥6×4 |
| 2 | 18–22 px | The name of a Wide or Large place |
| 3 | 13–15 px | Roles, children |
| 4 | 11–12 px | State line |

A Small place uses level 2 for its name if it fits, else level 3. Names are the directory's
file name; a project's name overrides it.

### 7.3 State colour, scarce and standardised

| Colour | Means | Where |
|---|---|---|
| neutral | normal | almost everything |
| cool accent | current — the `Projects1` current project's root, or the single highest-effective context | one place at most |
| green, briefly | something completed (a `History1` file-operation with this root, in the last minute) | edge tint, fades at next tending |
| amber | attention/degraded (an index root under this path is `Stale`/`Unverified`; a mount is missing) | state line + edge |
| red | failure / action required (a journalled operation on this root failed; the root is gone) | state line + edge |
| grey | offline/unavailable (the root is on an unmounted volume) | whole tile dimmed; still placed, still removable |

Only the tile's edge and state line carry colour; the material does not change. If everything
is neutral, the screenshot is neutral, and a single amber place is findable.

### 7.4 Material identity (later phase)

The recorded decision already allows "optional material texture (glass-, stone- or
paper-like)… themes own the exact material palette". The garden narrows *what the material
identifies*: not the tile's kind, but **the place**. `material_seed` is derived from the index
`EntryId` when the root is indexed (so it survives a rename) and from the path bytes
otherwise. The theme maps the seed to a subtle deterministic pattern within its palette
(monotone: paper grain; Temple: mineral). `Tile.accent: Option<u32>` already exists and can
carry the seed's hue offset until a `material` field is added. `Templar` looks like `Templar` when it
regrows in six weeks. The surrounding field stays a plain mineral-paper substrate — "quiet OS
+ distinctive projects".

Ordering, per both `garden.md` and the 2026-09-01 review: hierarchy, spacing and typography
first; material only once §12 Phase 4 has proven the philosophy.

---

## 8. Manual gardening

Every Place has, in its context menu and the cheat sheet and the `templar` CLI (one declaration
in `templar-types/src/action.rs` drives all three, as the existing gate enforces):

| Action | Effect on the context record | Effect on the tile |
|---|---|---|
| `garden.pin` | `pinned = true`; landing = current | never removed or shrunk; may still grow if not user-sized |
| `garden.unpin` | `pinned = false` | decay resumes from now |
| `garden.keep` | `pinned = true`, `user_span = current` | frozen in size and place |
| `garden.hide` | `hidden = true` | removed now; never regrows; listed under Settings → Home → Hidden places, with Unhide |
| `garden.remove` | context deleted (record dropped) | removed now; can regrow later from fresh activity |
| `garden.grow` / `garden.shrink` | `user_span = requested` | the existing `field.resize` route; from then on the tender does not resize it |
| `garden.why` | — | shows the last tending reason for this tile and its current effective score, breadth, stage and dwell |
| `garden.tend` (field-level) | — | runs the planner now |
| `garden.forget-all` (field-level, confirmed) | ledger cleared except pinned | — |

Settings (`templar-session-core/src/settings.rs`, `shell.home-garden.*`):

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` for the reference user; see §14 | Off: no observation, no tending; existing Places stay as ordinary Folder tiles |
| `half-life` | `3d` | §5.4 |
| `max-contexts` | `64` | §5.4 |
| `ignore` | the §5.3 list | additive; a `!` prefix removes a default |
| `home-corner` | `bottom-left` | §6 rule 4 |
| `observe.index` | `true` | whether the index change log is read at tending moments |
| `observe.launches` | `true` | whether launches with a directory are recorded |
| `observe.opens` | `true` | whether brokered/portal opens are recorded |

The three `observe.*` switches are the privacy controls: each source can be turned off on its
own, and a person who wants Home to reflect only what they explicitly placed turns all three
off and keeps the garden's presentation.

---

## 9. Where the code goes

Per `ARCHITECTURE.md`: pure crates, thin hosts; `interfaces/` first, then code; shipped
signatures frozen; no new process.

### 9.1 Crates and modules

| Location | Contents | Rules it obeys |
|---|---|---|
| `crates/templar-session-core/src/homegarden.rs` | `HomeActivity`, `HomeContext`, the ledger (`observe`, `effective`, `stage_for`, root attribution given a `RootProbe` trait, ignore list, coalescing, bounds) | I/O-free policy core like its siblings; the daemon supplies a `RootProbe` that does the `stat`-class checks and a `ProjectRoots` lookup |
| `crates/templar-field/src/garden.rs` | The planner: `tend(&FieldDocument, &[ContextView], GardenConfig, now) -> Vec<(FieldOp, Reason)>`; `ContextView` is the small projection the planner needs (root, stage, landing, pinned, user_span) so `templar-field` does not depend on session-core | Toolkit-free, async-free (`deps-check` already guards `templar-field`); property tests in `tests/field.rs` |
| `crates/templar-state/src/migrations/` | One additive migration: `home_garden_contexts` (root bytes, JSON record) and `home_garden_events` (bucketed) tables | Backward-readable: an older sessiond ignores the tables |
| `services/templar-sessiond/src/apis/homegarden.rs` | `org.templaros.HomeGarden1`; runs the planner at tending moments and applies its ops through the existing `homefield.rs` apply path so undo covers them | Same crash unit as `HomeField1` (ADR-0001), same store, no cross-daemon race |
| `services/templar-filed/src/` (small) | At `HomeGarden1.Tend`, sessiond asks filed for `ChangesSince(seq)`; filed does not learn about the garden. When the index signals are wired (`@templaros.observation.index-change-signals-never-fire`), sessiond subscribes instead of asking. | filed stays ignorant of Home; the dependency runs one way |
| `shell/templar-shell/src/field_model.rs` | `merge` gains nothing garden-specific: the tender's ops arrive through `HomeField1.Changed` like any other client's. The shell records `DirectoryOpened` from folder windows and `LaunchedHere` from `place.*` actions by calling `HomeGarden1.Observe`. | The shell owns no layout state (HomeField1's design) |
| `shell/templar-shell/src/surface/field_screen.rs` | The Folder tile's Wide/Large bodies; the `garden.*` verbs; the tending report row in `field.help` | WS3's hover-revealed controls |
| `crates/templar-types/src/action.rs` | `place.open-terminal`, `place.open-files`, `place.open-editor`, `garden.*` | one declaration, three surfaces |
| `interfaces/dbus/org.templaros.HomeGarden1.xml` | new interface (additive; nothing shipped changes) | `check-interfaces`, `interface-baseline` |
| `interfaces/schemas/settings.schema.json` | `shell.home-garden.*` | — |
| `docs/architecture/homegarden.md` | the readable architecture note | tree-check |

No new crate is proposed. If `homegarden.rs` grows past the size of `projects.rs` it can become
`crates/templar-garden` later; the planner stays in `templar-field` regardless, because it emits
`FieldOp`s and must share the field's invariants and tests.

### 9.2 The interface

```xml
<!-- Bus: org.templaros.Session. Path: /org/templaros/Session/HomeGarden.
     The activity ledger behind Home's grown places (docs/architecture/homegarden.md).
     Observation is fire-and-forget; tending is explicit; every change to the field goes
     through HomeField1 so it is undoable there. -->
<interface name="org.templaros.HomeGarden1">
  <!-- kind is one of the ActivityKind strings; role may be "". path_b64 is native bytes. -->
  <method name="Observe">
    <arg name="kind" type="s" direction="in"/>
    <arg name="path_b64" type="s" direction="in"/>
    <arg name="app_id" type="s" direction="in"/>
    <arg name="role" type="s" direction="in"/>
  </method>
  <!-- Run the planner for `output` now; returns the tending report as JSON
       (ops applied, each with its reason) — empty when nothing changed. -->
  <method name="Tend">
    <arg name="output" type="s" direction="in"/>
    <arg name="report_json" type="s" direction="out"/>
  </method>
  <!-- Every context with its effective score at now, stage, dwell, roles, children. -->
  <method name="Contexts">
    <arg name="contexts_json" type="s" direction="out"/>
  </method>
  <!-- One context by root: what `garden.why` shows. -->
  <method name="Describe">
    <arg name="path_b64" type="s" direction="in"/>
    <arg name="context_json" type="s" direction="out"/>
  </method>
  <method name="Pin"><arg name="path_b64" type="s" direction="in"/><arg name="pinned" type="b" direction="in"/></method>
  <method name="Hide"><arg name="path_b64" type="s" direction="in"/><arg name="hidden" type="b" direction="in"/></method>
  <method name="Forget"><arg name="path_b64" type="s" direction="in"/></method>
  <method name="ForgetAll"/>
  <!-- A tending moment changed the field; HomeField1.Changed follows for the ops themselves. -->
  <signal name="Tended"><arg name="output" type="s"/><arg name="changed" type="u"/></signal>
</interface>
```

`Observe` is deliberately a method with no return: a client that cannot reach sessiond loses an
observation and nothing else. Paths cross as base64 native bytes, the index's convention, so a
directory whose name is not UTF-8 is still a Place.

### 9.3 Persistence and migration

- Context records live in sessiond's state DB, keyed by root bytes, JSON value, versioned.
- The `FieldDocument` is untouched: the garden adds no field to the document format. A
  garden-owned tile is recognised by the ledger (its root is a context), not by a flag in the
  document, so `DOCUMENT_VERSION` stays 1 and no migration of saved Homes is needed.
- A context whose root no longer exists (index `Removed`, or `stat` fails at a tending moment)
  goes grey for one tending moment and is removed at the next, with the record retained
  Dormant for 30 days so a re-mounted drive brings its places back.

---

## 10. Resource, privacy and no-polling budget

| Concern | Rule |
|---|---|
| Idle wakeups | Zero. No timer anywhere. Observation is push; tending is on reveal. The `idle-wakeups.sh` measurement must show no change with the garden enabled. |
| Cost at reveal | `Contexts` is ≤64 records; `tend` is O(contexts × candidate spans) through `find_resize`; `ChangesSince` is one bounded page per tending moment (`limit` 2,000), and if `more` is true the rest waits for the next moment. Target: tending adds < 5 ms to a reveal on the reference laptop, measured the way `home-refresh-costs-profiled` did. |
| Storage | ≤64 contexts × ~1 KiB; event buckets pruned at 14 days. |
| I/O | `stat`-class checks on ≤8 ancestors per *new* unique path, cached per index generation. No file contents, ever. No directory walks. |
| Privacy | The ledger records paths and app ids the user already produced elsewhere (Recents, History, the index). Three per-source switches; per-path Hide; Forget-all. The ledger is in the same per-user store as Recents, with the same protection. Nothing leaves the machine. |
| Failure | Sessiond down: no observations, no tending, Home is the last saved field. Filed down: index source silent, others unaffected. Planner refused by `HomeField1.Apply` (a `FieldError`): the op is dropped and the reason logged; the field is never left half-tended because each op is independent. |
| Compositor | Untouched. Nothing here crosses the shell/compositor boundary. |

---

## 11. Subtractions this plan interacts with

`garden.md`'s first half lists deletions; most are already decided or under way, and one is not.
The garden does not depend on them, but the *feel* the user is after does.

| Deletion in `garden.md` | State in the ledger | This plan |
|---|---|---|
| Running windows leave Home; the taskbar owns them | Decided (`home-projects-and-tool-actions`); the bottom bar exists (Lane B); `home-taskbar-migration` acceptance is open | Dependency: the garden's home corner competes with parked-session tiles for the same slack until they migrate. Finish the migration first, keeping explicit park/group relationships. |
| The permanent Desktop listing | Replaced by the Desktop folder tray (decided) | Kept; §14 question. |
| The Session box | `session` is the first of three system widgets, kept because "an empty shelf has no route to logging out" | **Proposed:** the bar gains a power glyph opening Lock/Suspend/Restart/Shut down/Log out (the existing `session.*` actions), and the Session widget is migrated off the default field by the same mechanism `migrate_launcher_tile` used. Requires the user's decision (§14) and a revision of `@templaros.work.home-widgets`'s "every capability arrives as a widget". |
| The Settings card | The Settings widget (battery/volume/brightness + a door to the app) is the "third and last" system widget | **Proposed:** the three readings move to the bar's indicator row (Lane B already declares indicators; nm-tray2 already lives there); Settings remains an application reached from the bar and Apps. The widget contract stays for notifications and any future *information-first* instrument. The user named this exact case. |
| Notifications "All caught up" | The notifications widget renders sessiond's server | **Proposed:** default state is absence: no tile when the queue is empty; a transient card at the top-right on arrival; history behind a bar indicator. Requires revising the widget's descriptor, not the contract. |
| The app-icon strip | Retired (`migrate_launcher_tile`); the Apps bar is the launcher | Done. |
| The "Home" label | Not present in the field (the bar names it) | Nothing to do. |
| Performance statistics on the front page | None exist; `resourced` surfaces itself only when exceptional (2026-09-01 review §13) | Recorded here as a standing rule: no CPU/RAM/disk gauge is ever a default Home object. Pressure promotes a state colour on the affected place or session, nothing more. |

With those, the permanent contents of Home are exactly: the Apps bar, Files, Desktop, whatever
the user pinned — and the garden.

---

## 12. Phases and acceptance

Each phase is one AKR work item; acceptance checks use the ledger's three methods. Phases 1–3
are pure and testable without a display. Phase 4 is where the philosophy is judged.

### Phase 0 — Decisions and subtraction (`@templaros.decision.homegarden-activity-grown-places`)

- Record the decision in §3.1's words; `knowledge.revise` the consequences of
  `home-field-stable-bounded-semantic-grammar` to define "content-driven" as index/content
  events and to admit user-action-driven size change at tending moments; supersede the
  "repack on activity" clause of `home-developer-setup-preview` with a disposition that keeps
  its explicit preview as an orthogonal route.
- Register `garden.md` as a source.
- Decide §14's questions or leave them as `knowledge.question`s.

Acceptance: `decision-recorded` (observation); `contradictions-resolved` (command:
`knowledge.validate` clean).

### Phase 1 — The activity ledger (`@templaros.work.homegarden-ledger`)

- `templar-session-core::homegarden`: types, ignore list, coalescing, root attribution behind
  `RootProbe`, effective score, stage machine.
- `HomeGarden1` interface and sessiond host; state migration.
- Sources: `Launcher1` launches whose arguments name a directory (`--directory`, a bare path
  argument that is a directory); `Projects1.SetCurrent`; `Recents1.Record` forwarded in
  sessiond? — no: Recents lives in filed. Instead the shell's `brokered_file_open` and
  `templar-files` call `Observe` beside `Record`. Folder windows call `Observe(DirectoryOpened)`
  on navigation. `PlaceFolder` records `PlacedOnHome`.
- `templar garden contexts` / `templar garden why <path>` in the CLI, so the ledger can be read
  without the shell.

Acceptance: `ledger-is-pure` (command: `deps-check`, no I/O in the core); `breadth-beats-count`
(command: a test where 300 saves score below 3 opens + 1 launch across 2 days);
`decay-is-arithmetic` (command: no timer, no spawned task in the host — a test that the host
registers no interval); `ignore-list-holds` (command: 40,000 `target/` changes produce no
context); `bounded` (command: 65th context evicts the lowest Dormant).

### Phase 2 — The planner (`@templaros.work.homegarden-planner`)

- `templar-field::garden::tend` and `ContextView`.
- Every invariant in §6 as a proptest.
- `Tend` on sessiond applies through the `HomeField1` path; undo covers it; the tending report
  is a `History1` event.

Acceptance: `never-moves` (command); `user-tiles-untouched` (command); `idempotent` (command);
`dwell-respected` (command); `resize-yields-garden-only` (command); `undoable` (command: a
tended field undone is byte-identical to before).

### Phase 3 — Tending moments and the shell (`@templaros.work.homegarden-shell`)

- Reveal and session start call `Tend` for the revealed output before paint.
- `garden.*` and `place.*` actions in the manifest; context menu, cheat sheet, CLI.
- The Folder tile's Wide and Large bodies (§7.1–7.3); `--render` fixtures for a Small, Wide
  and Large place, and a visual baseline hash.
- Settings keys and the Hidden places list in the Settings app.

Acceptance: `render-fixtures` (command); `one-declaration-three-surfaces` (command, existing
gate); `reveal-cost` (command: tending adds < 5 ms measured with the existing profile method);
`no-idle-wakeups` (command: `scripts/idle-wakeups.sh` unchanged with the garden enabled).

### Phase 4 — Hands-on: does the philosophy work? (`@templaros.work.homegarden-hands-on`)

The test `garden.md` proposes: build the model, three sizes, let thresholds choose, and see.

- The user works for a day on the installed desktop with the garden enabled and the sources
  of Phase 1.
- At the end of the day, Home shows the three to five directories they actually worked in,
  sized by how much; nothing they did not touch; no tile moved that they did not move.

Acceptance: `garden-matches-the-day` (manual — the user's verdict); `nothing-moved-uninvited`
(observation: the tending reports for the day contain no `Move` and no change to user tiles);
`explainable` (manual: `garden.why` on every place gives a reason the user recognises).

If this fails, the plan stops here and the reasons go into the ledger. Nothing in Phases 5–6
is worth building on a model that does not match a day.

### Phase 5 — Widening the sources (`@templaros.work.homegarden-sources`)

- The index change log at tending moments (`HumanFileModified`, `ArrivedHere`), with the
  Downloads `3 new` state line.
- The FileChooser portal (after X1) calling `Observe`.
- The `CodeEditor` role, so the third action exists.
- Index signals, once wired, replacing the on-demand read.

Acceptance: `index-never-moves` (command: a burst of index changes yields no `Place`/`Resize`
above the medium-weight cap); `portal-observes` (command, after X1).

### Phase 6 — Identity and bridges (`@templaros.work.homegarden-identity`)

- Material seed per place (§7.4), theme-owned rendering, regrowth restores it.
- The seven-day **Make this a project** row.
- The Project tile's Large body shares the roles-and-children renderer.
- A treemap band inside a ≥5×4 place.

Acceptance: `material-survives-rename` (command); `promotion-is-a-row-not-a-prompt`
(observation).

### Dependencies on existing work

| This plan needs | From |
|---|---|
| Parked sessions migrated to the bar so the home corner has slack | `@templaros.work.home-project-tool-actions` (`home-taskbar-migration`) |
| Hover-revealed manipulation on Small tiles | `@templaros.work.representation-chrome-and-continuity` (WS3, mostly done) |
| Folder windows reusing the Files surface state | `home-vision-review-20260905`'s carried priority |
| Index change signals wired | `@templaros.observation.index-change-signals-never-fire` |
| FileChooser portal | `@templaros.work.developer-edition-daily-driver` (X1) |
| `CodeEditor` role | `@templaros.work.home-project-tool-actions` |
| Bar indicators for battery/volume/brightness and a power menu | Lane B, plus the §14 decision |

---

## 13. What is deliberately not done

From `garden.md`, rejected or deferred with reasons:

| Idea | Disposition | Reason |
|---|---|---|
| Sibling grouping: four active siblings under `/data/code` form a `CODE` group | **Deferred; explicit only** | Window-groups rule 1 and the constitution forbid inferred relations by proximity, application or time. The parent *directory* may itself become a Place if the user opens it; that is presence, not a group. An explicit folder-on-folder gesture making a Places tray is a possible later grammar row. |
| Radial "sprouts" of applications around a folder | **Rejected** | Rectangles only; roles are a row inside the tile. |
| "Search files, places, apps, commands…" as one omnibox | **Not reopened** | The user decided Apps-only on 2026-09-03 after the federated attempt failed. |
| Physics only during editing | **Already so** | `drag.rs` derives velocity; nothing animates at idle; `reduced-motion` exists. |
| Continuously simulated layout | **Rejected** | §6 rule 1. |
| Assuming a current project on first boot | **Rejected** | §5.6; the user's response. |
| A wizard: "We detected a project! Create project?" | **Rejected** | §5.7's row. |
| Observing terminal `cwd` after launch | **Rejected** | Needs hooks or tracing; launch directory is the honest signal. |
| Reading pcmanfm-qt's `recently-used.xbel` | **Deferred, as already recorded** | Two writers of one answer; and it answers "what did I open", which the brokered route already covers for Home. |
| Weather, travel, calendar widgets | **Out of scope** | The user: "usually not important on desktop". The widget contract remains for anyone who wants them; none is default. |
| Making tile area proportional to bytes | **Rejected** | Both reviews and the constitution: treemaps live inside storage views. |
| An infinite or panning Home | **Rejected** | Bounded field; nothing can be lost. |

---

## 14. Open questions for the user

To be recorded as `knowledge.question`s if not answered:

1. **Default on?** The plan defaults `shell.home-garden.enabled` to `true` because a garden
   that is off is a Home that stays empty, which is the finding this answers. The cost is that
   Home changes after a day of use without the user having asked for a feature. Alternative:
   off by default, with one row in the first-run notes.
2. **Should the Desktop tray be an ordinary pinned Place** (so it can be unpinned and recede)
   rather than a permanent default? The recorded decision made it permanent on 2026-09-04.
3. **May the tender shrink another *garden* tile to make room** (rule 3 as written), or only
   grow into empty cells? Shrinking garden neighbours makes the field denser; growing only
   into empty space makes it calmer and more predictable. The plan leans to the former with
   a strict garden-only yield set; the user may prefer the latter.
4. **Session and Settings: widget or bar?** §11's proposal moves both to the bar. This is the
   user's own stated preference in `garden.md`, but it revises `@templaros.work.home-widgets`.
5. **Half-life.** Three days is a guess for a person who works in a few repositories for
   weeks. A writer moving between many documents may want seven; a person with a weekly
   rhythm may want ten.
6. **Home corner.** Bottom-left, opposite Files? Or should the first growth land beside the
   Desktop tray?
7. **Naming.** "Homegarden" is the internal name for the model and the work items. Does the
   surface stay "Home"? (The plan assumes yes; nothing user-facing says garden except the
   `garden.*` verbs, which could be renamed `home.pin`, `home.hide`… to match `home.reveal`.)

---

## 15. How to know it worked

`garden.md` ends: "That alone is enough to determine whether the philosophy works." The
measure is Phase 4, and it is qualitative on purpose. Three quantitative companions:

- **Precision:** of the places on Home at the end of a day, the fraction the user recognises
  as places they worked. Target: all of them.
- **Recall:** of the directories the user names when asked "where did you work today", the
  fraction that are on Home. Target: the top three, always.
- **Stability:** across a week, the number of user-visible tile position changes not caused
  by the user. Target: zero — this is a hard invariant, not a target, but it is also the
  number to report.

And one qualitative sign that it is *right* rather than merely working: after a week, the user
stops placing folders on Home by hand, because the ones they need are already there.

---

## Appendix A — A worked day

Cold Home, morning: Apps bar; Files (top-left, 3×3); Desktop tray (2×2, top-right); bar below.
Nothing else.

09:10 — The user presses Super, types `kitty`, launches it. No directory argument: no event.

09:12 — In kitty they `cd ~/src/templaros` and build. The garden sees nothing (no cwd tracing;
`target/` ignored). Honest.

09:40 — They open a folder window on `~/src/templaros/docs/planning` from Files.
`DirectoryOpened(~/src/templaros/docs/planning)` → attributed to `~/src/templaros` (rule 3: `.git`
present) → context created, score 2.0, child candidate `docs/`.

09:41 — They open `homegarden.md` from that window. `FileOpened` → +1.5, child `docs/`.

10:30 — They right-click the folder window → Open terminal here. `LaunchedHere` with role
`Terminal` → +3.0, roles += Terminal. Effective ≈ 6.5. Stage: Trace (≥3.0). No tile yet: no
tending moment has passed.

10:31 — They press Super. **Tending moment.** `tend` emits `Place { Folder ~/src/templaros, Small
2×2 }` at the home corner, reason `new: 2 opens, 1 terminal today`. Home now has a fourth
object: `templaros`, in its own material, state line `today`.

11:00–17:00 — Twenty more opens under `crates/templar-field` and `docs/`, two more terminals, one
editor launch on a file (role CodeEditor, once that role exists). Score grows past 20 but the
day-count is 1, so Tile requires the second day.

Next morning, 09:05 — Super. Effective, after 16 hours of decay from ≈30: ≈ 24. Two distinct
days now. Trace → Tile (dwell ≥ 1 h satisfied). `Resize` to 3×2 — `find_resize` finds the room
in empty cells; reason `grew: 23 opens, 3 terminals, 1 editor, 2 days`. The tile now shows
`>_ Terminal  ▤ Files  <> Edit`.

Day three, afternoon — Super. Three days, two roles, effective ≈ 40. Tile → Expanded. `Resize`
to 4×3; children appear: `crates/`, `docs/`, `README.md`, `justfile`. The user drags the tile
to the left-centre where they want it. `Move` is theirs; `landing` is updated; the tile is
now user-touched: it can still grow (no `user_span`), never shrink.

Day five — They open `~/Downloads` once to fetch an ISO. `DirectoryOpened` on an XDG dir
(rule 5) → Trace; two `ArrivedHere` from the index. Next Super: a 2×2 `Downloads` beside
`templaros`, state line `2 new`.

Day twelve — `Downloads` untouched since day five: effective < 1.0, dwell 5 days passed →
Dormant → `Remove`, reason `no activity for 7 days`. `templaros` still Expanded. The context menu
on `templaros` has gained **Make this a project**. The user ignores it. Fine.

Six weeks later — The user returns from a trip. `templaros` decayed to Trace on day 20 and
Dormant on day 27; Home is Files, Desktop, and nothing. They open a terminal in `~/src/templaros`
from Files. Next Super: `templaros` regrows, 2×2, *in the left-centre cell they moved it to*, in
the same paper grain. The garden remembered.

---

## Appendix B — `garden.md` point → plan section

| `garden.md` | Section |
|---|---|
| Put the important thing in the middle | §5.6 (nothing is fabricated), §6 rule 4 (growths cluster at a corner the user learns) |
| Remove boxes; use spacing, typography, material | §7.2–7.4; WS3 chrome |
| Delete the Desktop widget | §3.2, §14 Q2 |
| Delete the Session panel; account/power control | §11 |
| Notifications: absence by default | §11 |
| Running windows leave Home | §11, dependency |
| Launcher as search/command | §3.2 (Apps-only kept) |
| Alignment grid | Exists: `TILE_SIZE` 88, `GAP` 6 |
| Intentional empty space | §5.6, §6 rule 4 |
| Projects as the strongest objects; material for persistent identity | §7.4 |
| Current project larger than others | §5.4 stages, §7.3 cool accent |
| A project field, not a rigid dashboard | The field already is one |
| Physics only during editing | §13 (already so) |
| Project groups as regions | §13 (deferred, explicit only) |
| Four type levels | §7.2 |
| Don't overuse icons | §7.1 roles row |
| Scarce, standard state colours | §7.3 |
| System information as one quiet edge | §11 (the bar) |
| Home button redundant | nothing to do |
| Home slightly darker/plain substrate | §7.4, theme-owned |
| Roots / growths / pinned | §4, §5.6, §5.4 |
| What makes a folder grow; breadth over count | §5.1, §5.4 |
| Use roles, not application names | §5.5 |
| Contexts without declared projects; no wizard | §5.7 |
| Zero-polling decay | §5.4, §10 |
| Spatial persistence | §6 rules 2–5 |
| Mostly filesystem-derived Home | §5.6 |
| Sparse first boot | §5.6 |
| Files as a gateway | §5.6 |
| Children emerge | §5.5, §7.1 |
| Apps grow out of places | §5.5 |
| Recursive (sibling) grouping, conservatively | §13 (deferred) |
| Manual gardening | §8 |
| Three visibility states | §4 |
| Material belongs to places | §7.4 |
| Four concepts only | §4 |
| Build the activity model first; three sizes; let thresholds choose | §12 Phases 1–4 |
| "Homegarden" as the internal name | §0, §14 Q7 |
