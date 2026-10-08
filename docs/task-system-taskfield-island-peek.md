# The Task System — one window model, four scales, and Taskfield

*Planning document, 2026-09-09. Extrapolated from the external review "TemplarOS does need a
taskbar, but the taskbar should not be responsible for making every window discoverable"
(registered as an AKR source beside `homefield-design-advice` and
`design-review-home-and-platform-20260901`). This retained review uses historical
`lege-comp`/`lege-shell`/`lege-window` names; the current workspace uses
`templar-comp`/`templar-shell`/`templar-window`. The September proposal below is historical
rationale. Current authority is `legeos.decision.task-system-four-scales-one-model` and
`legeos.decision.homegarden-activity-grown-places/3` in AKR.*

**October 7 owner revision:** Home contains project-associated live and parked windows,
optional previews and sessions, with unlinked windows in one Loose tray. The counts-only,
no-window-list/no-preview and explicit-park-only clauses below are superseded by the owner's
PDF. Project context is verified separately from explicit compositor groups. Taskfield remains
a transient global view; it does not own persistent Home geometry. Current acceptance is
tracked by `legeos.work.pdf-homegarden-hdmi-20261007`, not the old `home-lists-no-windows` check.

---

## 0. The idea in one paragraph

Stop asking the bar to be tiny, permanent, fullscreen-compatible and able to represent sixty
windows. Give window management **four scales, each answering one question and none
duplicating another**: HomeGarden asks *what am I working on*, the **Task Island** (the bar)
asks *what is running*, **Taskfield** asks *where are all my windows*, and the Apps bar asks
*what can I start*. Alt+Tab is the small, fast version of Taskfield for *what was I just
using*. All four read **one authoritative TaskModel** in `lege-window`, so they cannot disagree
about identity, grouping or recency. The novel piece is Taskfield: not a shell overlay fighting
a fullscreen window for stacking and input, but a **presentation mode of the compositor
itself** — the scene pulls back and the live windows become the interface, laid out once,
frozen for the session, with semantic zoom from task units down to single windows. Fullscreen
stops being a special case: it wins only while no presentation override is active, and the
route out of it is always the same two gestures — `Super+Tab`, or a corner dwell that opens
**Peek** and then Taskfield.

Most of the substrate exists. `lege-window` is already the sole stacking authority with
groups, shelving, `cycle_focus`, `cycle_within_group`, `cycle_groups` and a
`scatter_layout`; the compositor already has a shell-independent "show every window" view, a
fallback switcher HUD, and a preview capture path that works for hidden windows; the bar is
already a chip island that grows only when something needs the width. What is missing is a
shared model with recency in it, a presentation mode that draws the windows rather than cards
for them, a fan instead of a row, Peek, and the bridges to Home and the launcher.

---

## 1. What exists today, honestly

| Surface | Where | What it does now | What is wrong with it for scale |
|---|---|---|---|
| Bottom bar | `shell/lege-shell/src/surface/bar_screen.rs`, `taskbar.rs`, `dock.rs` | Chips at the left (network, clock, one per entry); groups first in group-id order, then one entry per application via `dock::group(ByApplication)`; a one-window app's chip carries its *title*; the bar is only as wide as its chips unless a maximized window's controls or a hover row need the full width; exclusive zone `BAR_HEIGHT` 48; **dropped entirely while a window is fullscreen** (Lane B) | A dwell opens a *row* of `ROW_ITEM_WIDTH` 220 cells, one per window, with no upper bound — the cosmic-app-list behaviour the review calls ridiculous at 24 windows. The active chip shows a title. Under fullscreen there is no pointer route to anything. |
| Alt+Tab | `lege-comp/src/keybindings.rs` → `WindowService::cycle_within_group`; compositor-drawn `switcher` HUD | Cycles inside the focused window's group, with the loose set counting as a group; the HUD is compositor chrome | Order is stacking order, not recency — there is no MRU anywhere in the system, only `LegeCompState::previous_focus` |
| Super+Tab | same → `cycle_groups` | Lands on each group's `last_active` member | Decided by `@legeos.decision.window-groups-are-explicit-and-compositor-owned`; the review wants this chord for Taskfield |
| Show every window | `Ctrl+Alt+Space` → `WindowCommand::ShowAllWindows` → `geometry::scatter_layout`; `chrome::collect_scatter` | A recovery view that works with the shell dead; round-robin per output, row-major grid | Draws a **coloured card and a label per window**, not the window. It is a map of where windows are, not a switcher; it cannot activate by pointer, has no grouping, no search, no zoom. |
| Fallback switcher HUD | `chrome::SwitcherState` | A compositor-drawn list for the two cycle chords | Minimal by design; the right floor for Taskfield's shell-dead mode |
| Window groups | `lege-window/src/group.rs`, `service.rs`; `lege-comp/src/window_groups.rs`; protocol v3/v5 | Explicit, compositor-owned, stacked and shelved as one, outlined, one chip on the bar, one container tile on Home | Finished (WG1–4); the hands-on acceptance is open |
| Shelf (park) | `WindowService::shelve/unshelve`, `shelf()`; `Ctrl+Alt+P`; the park control tag; Home's `ParkedSession` tiles | A shelved window is gone from the screen and appears as a tile on Home (`merge_sessions`) | `@legeos.decision.home-projects-and-tool-actions` says automatic parked-window shelves migrate to the bar; `home-taskbar-migration` acceptance is open. Today parked windows are still Home furniture. |
| Previews | `lege-comp/src/screencopy.rs` over `ext-image-copy-capture-v1`, trusted shell only; `shell/src/previews.rs` | Rate-limited, downscaled captures of any window **including hidden ones** ("a window that is hidden still previews") | Only the *shell* holds the images. A compositor-drawn view has no thumbnail cache of its own — but it has something better: the surfaces. |
| Home's window tiles | `field_model::merge_sessions`, `merge_window_groups` | Parked sessions and groups as tiles | A second window list computed in the shell, in the shell's own order |
| Fixed recovery chords | `keybindings.rs`, `docs/architecture/accessibility.md` | Show-all, escape fullscreen, cycle, home, park, step, group, terminal — all call `WindowService` directly, none need the shell | The right precedent: compositor-owned navigation that survives the shell dying |
| `ShellToplevel` | `lege-window/src/shell.rs` | id, app, instance, title, activated, fullscreen, shelved, maximized, geometry, layer, group | No activation serial, no map serial, no output, no urgency, no transient-for |

Four independent computations of "what are the windows and how do they group" exist:
`taskbar::entries`, `dock::group`, `field_model::merge_sessions`/`merge_window_groups`, and
`WindowService::group_cycle_targets`. None of them knows what the user used last.

---

## 2. The Task System

```text
HomeGarden      what am I working on?      places (implemented)
Task Island     what is running?           the bar, kept small on purpose
Taskfield       where are all my windows?  the compositor's presentation mode
Apps bar        what can I start?          Home's launcher (decided; unchanged)
Alt+Tab         what was I just using?     the seven most recent windows
```

Five rules, each with its reason:

1. **One question per scale.** The bar never lists windows; Home never lists windows;
   Taskfield never launches; the launcher never switches (it *activates*, §9). Reason:
   `@legeos.decision.representation-ladder-is-the-organizing-principle` — one object, one
   canonical route — applied to windows.
2. **One model.** Every scale reads `lege_window::TaskModel` (§4). Reason: four lists that
   drift is the bug this replaces.
3. **The bar is insufficient by design.** It shows coarse topology and gives fast access to it;
   it stops at four windows per unit and hands over to Taskfield. Reason: a linear strip is
   intrinsically bad at thirty windows, and pretending otherwise is what makes bars grow rows.
4. **Fullscreen wins only while nothing overrides it.** Passive chrome (bar, Home, tags) yields
   to fullscreen; explicit navigation (Taskfield, Alt+Tab, lock, recovery) yields to nothing.
   Reason: "I asked one application to occupy the display" and "I asked to see my windows"
   are different requests; the second must always be honoured in one deterministic step.
5. **Placement is frozen while you look.** Taskfield lays out once per session and only
   relayouts on a substantial filter change, a window closing, an explicit zoom, or an
   explicit regroup. Selection highlights; it never moves geometry. Reason: spatial memory —
   the same rule Home lives by.

---

## 3. The ledger it must live inside

| Recorded rule | Source | Verdict | How this plan honours or revises it |
|---|---|---|---|
| `lege-comp` is conservative and minimal; `lege-shell` is everything decorative and restartable | ADR-0012, `CLAUDE.md`, COMPOSITOR.md | **Kept, with a stated boundary** | Taskfield's *mode* — the scene transform, layout, input routing, selection, filtering — is `lege-window` policy hosted by the compositor, like show-all and the switcher HUD today. Its *rich chrome* (titles with icons, the search field, the place header) is a shell layer surface the compositor places only inside the mode. Shell dead ⇒ Taskfield still works with compositor labels (§5.7). This is the recovery-band precedent, not a new kind of thing. |
| `lege-window` is the sole z-order and visibility authority; the compositor never maintains stacking independently | COMPOSITOR.md §4 | **Kept** | Taskfield is a *presentation* over `stacking_order`, expressed as a `Presentation` value on `WindowService`; it does not touch records or ranks. Leaving the mode restores exactly the prior scene. |
| Recovery: show-all, escape, fallback switcher, go-home all work without the shell | COMPOSITOR.md §5, M2 `recovery-without-shell` | **Kept and strengthened** | Taskfield's shell-dead rendering *replaces* the scatter: `window.show-all` (`Ctrl+Alt+Space`) becomes "Taskfield, compositor-only". One view fewer, and the recovery route is the everyday route, so it is exercised daily instead of never. |
| `Alt+Tab` cycles within the focused group; `Super+Tab` cycles across groups; no `Ctrl+Tab` | `@legeos.decision.window-groups-are-explicit-and-compositor-owned`, planning doc §2 rule 4 | **Revised** | `Alt+Tab` becomes MRU across everything (≤7); `` Alt+` `` becomes "within the current task unit" — which *is* the within-group switcher when the focused window is grouped, and within-application otherwise, so the group feature still costs nothing until a group exists. `Super+Tab` becomes Taskfield, whose top level is *task units* — explicit groups first — so "across groups" is subsumed rather than lost. No `Ctrl+Tab`, still. Needs `knowledge.revise` on the decision's chord rule. |
| The field on bare Home is the launcher, Apps only; `launcher.search` has no chord | `@legeos.decision.the-home-field-is-a-launcher-not-a-search` | **Kept, given a chord** | `Super+Space` does not open a second launcher; it *reveals Home with the Apps bar focused and armed*, through the existing `reveal_home`. One launcher object, one more route into it. Under fullscreen, `reveal_home` already hides client windows, so it works there. |
| The bottom bar owns ordinary automatic per-window selection; automatic parked-window shelves migrate to it, keeping explicit group/park/restore relationships | `@legeos.decision.home-projects-and-tool-actions`; `home-taskbar-migration` open | **Fulfilled here** | Parked windows leave Home as automatic furniture (§7.5, §10.3); they remain on Home only as an *explicit* relationship (parked into a project or place). |
| Home is momentary chrome; bare Super toggles/latches it; Super chords never accidentally latch | `home-is-momentary-and-placeable-objects-are-containers` | **Kept** | `Super+Tab` is a chord, handled by `SuperTapTracker`'s existing chord-cancels-tap rule. |
| A shell surface's exclusive zone is a floor on thickness, nothing about extent; the bar is chips-wide | Lane B (`bottom-bar-taskbar/6`), COMPOSITOR.md zone row | **Kept** | The fan (§7.3) opens *inside* the bar surface exactly as the hover row does — the surface grows by a band, the zone stays `BAR_HEIGHT`. |
| A maximized window under the bar has no frame; the bar carries its controls | Lane A (`window-tags-replace-the-band/2`) | **Kept** | The island's right end still carries the maximized window's controls (§7.6). |
| System behaviour in services; tray icons, panels and applets are views; no tray | `@legeos.constraint.services-not-indicators` | **Kept** | The island's machine chips (network, clock, and the battery/volume/brightness moved there by the HomeGarden plan §11) are views of services. Separating them visually from the task chips (§7.7) changes nothing about ownership. |
| No polling; the compositor is the only honest idle source | `idle.rs`, measurements | **Kept** | The preview cache is damage-driven (§5.8). Taskfield input is input: `note_activity` runs. |
| Window previews: capture globals filtered to the trusted shell | `screencopy.rs` | **Kept** | The compositor draws surfaces directly in Taskfield; no capture crosses to any client. The shell's fan cards use the existing capture path. |
| `lege-window` carries no async runtime or D-Bus dependency | `deps-check` | **Kept** | `TaskModel`, `taskfield.rs` and the weighted packer are pure; `lege-treemap` has no dependencies at all and may be used for the unit packer if a squarified layout is wanted (§5.3). |

---

## 4. TaskModel — the one authoritative window model

Lives in `crates/lege-window/src/task.rs`, computed by `WindowService` from its own records, and
published to the shell through the existing snapshot. Nothing else computes grouping or order.

```rust
/// Monotonic per-service counters, never wall-clock: the task system needs its own temporal
/// truth and must not derive recency from stacking order.
pub struct TaskWindow {
    pub id: WindowId,
    pub app_id: Option<AppId>,
    pub instance_id: Option<InstanceId>,
    pub title: String,
    pub activation_seq: u64,   // bumped on every successful Activate; 0 = never
    pub mapped_seq: u64,       // bumped on map
    pub fullscreen: bool,
    pub maximized: bool,
    pub shelved: bool,
    pub urgent: bool,          // xdg-activation request refused for lack of input evidence
    pub transient_for: Option<WindowId>,
    pub output: Option<OutputId>, // from geometry via choose_target_output, recomputed on move
    pub group: Option<GroupId>,
}

pub enum TaskKey {
    ExplicitGroup(GroupId),
    Application(AppId),
    Unknown(WindowId),         // unverified identity: one unit per window, never merged
}

pub struct TaskUnit {
    pub key: TaskKey,
    pub windows: Vec<WindowId>, // by activation_seq desc, then mapped_seq
    pub last_active_seq: u64,
    pub any_shelved: bool,
    pub all_shelved: bool,
    pub accent: Option<u32>,    // the group's; None for an application
}

pub struct TaskModel {
    pub generation: u64,
    pub windows: BTreeMap<WindowId, TaskWindow>,
    pub units: Vec<TaskUnit>,   // explicit groups first (group-id order), then apps (key order)
    pub active: Option<WindowId>,
}

pub enum EntryPresentation { Direct, Fan, Taskfield }

pub fn presentation(unit: &TaskUnit) -> EntryPresentation {
    match unit.windows.iter().filter(|w| !shelved(w)).count() {
        0 | 1 => EntryPresentation::Direct,
        2..=4 => EntryPresentation::Fan,
        _ => EntryPresentation::Taskfield,
    }
}

/// The seven most recently activated visible windows, newest first, excluding `current`.
pub fn recent(model: &TaskModel, current: Option<WindowId>, limit: usize) -> Vec<WindowId>;
/// Windows of the unit `current` belongs to, by activation_seq — Alt+` and the fan.
pub fn within_unit(model: &TaskModel, current: WindowId) -> Vec<WindowId>;
```

Rules:

- **Unit order is stable, not MRU.** Units keep their first-seen position for as long as they
  exist; recency reorders *inside* a unit only. Reason: the island's chips must not swim.
  A unit that disappears (last window closed) leaves a gap the next new unit fills.
- **`activation_seq` is set by `WindowService::activate`** — the one place activation is
  granted — so `cycle_focus`, the launcher's Enter, the fan and Alt+Tab all see the same order.
- **Unverified identity never merges.** A window whose cgroup identity is unverified is its
  own unit (`Unknown`), drawn without an icon. Reason: the identity invariant — a claimed
  `app_id` must not join a trusted application's chip.
- **Transients ride with their parent.** A `transient_for` window is not its own unit member;
  it is shown inside its parent's card in Taskfield and never in the bar.
- **`ShellToplevel` gains** `activation_seq`, `mapped_seq`, `urgent`, `transient_for`,
  `output` as `lege_shell_toplevel_v1` **version 4** events, additive; `shelf_rank` is finally
  wired from `activation_seq` of the shelve, closing the note in `drawers/window_shelf.rs`.

The shell's `taskbar::entries`, `dock::group`, `field_model::merge_sessions` and
`merge_window_groups` are rewritten as thin projections of `TaskModel::units` (the shell
rebuilds the model from `ShellToplevel`s with the same pure function, so a test constructs it
directly, as today). The compositor's `group_cycle_targets` is deleted in favour of
`TaskModel::units`.

---

## 5. Taskfield — the compositor's presentation mode

### 5.1 One presentation state instead of five flags

`LegeCompState` today carries `show_all_windows`, `switcher`, `home_revealed` (via the
service), `session_lock`, `recovery_protocol` state and `arrangement_active` as separate
fields, each with its own interaction with fullscreen and with each other (the comment at
`keybindings.rs:665` already documents that the scatter and the HUD cannot coexist with a
reveal). Replace them with one value on `WindowService`:

```rust
pub enum Presentation {
    Normal,
    HomeReveal,                 // the existing reveal_home view flag
    QuickSwitch(SwitchState),   // Alt+Tab / Alt+` HUD
    Taskfield(TaskfieldState),
    Locked,                     // ext-session-lock
    Recovery,                   // lege_recovery_v1 pressure overlay
}

pub struct TaskfieldState {
    pub entered_from: Option<WindowId>,
    pub layout: TaskfieldLayout,   // frozen (§5.3)
    pub zoom: Zoom,                // Units | Unit(TaskKey) | Window(WindowId)
    pub filter: TaskFilter,        // All | Unit(TaskKey) | Windows(Vec<WindowId>) | Shelved
    pub query: String,
    pub selected: Option<WindowId>,
    pub generation: u64,
}
```

The invariant that replaces every fullscreen special case:

> **A fullscreen window covers passive chrome only while `Presentation::Normal`.** In every
> other state, `is_visible`, hit-testing and the scene are answered by that state.

`home_is_covered` gains the same guard, so Home is never hidden by a fullscreen window during
a reveal (already true today by a separate rule, now by construction). Transitions are
explicit and total: `Normal → Taskfield` on `Super+Tab`, corner Peek, `window.show-all`, a
HomeGarden "N windows" press, a fan overflow, or the launcher's `→`; `Taskfield → Normal` on
activate, `Escape`, `Super+Tab` again, or the shell disconnecting mid-mode. `Locked` and
`Recovery` pre-empt everything; `Taskfield` pre-empts `HomeReveal` and `QuickSwitch`.

### 5.2 What the scene does

Entering Taskfield does not reconfigure a single client. The compositor renders every visible
client surface tree at a **presentation rectangle** instead of its real one — the same
mechanism `screencopy::drain_pending` uses to draw a surface tree offscreen, now drawn onscreen
scaled. No `xdg_toplevel.configure` is sent; clients keep painting at their real sizes; leaving
the mode is a render change, not a geometry change. This is why it works from fullscreen every
time: the fullscreen window is just a surface drawn small.

The entering animation is **in-place pull-back**: every window shrinks from its real rect
toward its presentation rect over the reduced-motion-aware duration Home's tile expansion
already uses (`WS3` expansion interpolation). Peek (§6) is the first 5–8% of this same
interpolation, paused. Reduced motion removes the intermediate frames. Leaving reverses it.

### 5.3 Layout: frozen, weighted, per output, in place

`lege_window::taskfield::layout(&TaskModel, &[Output], zoom, filter) -> TaskfieldLayout` is
pure and property-tested:

- **Per output, in place.** Each output packs the windows that are on it (`TaskWindow::output`),
  never round-robin — a window does not jump screens to be found. Shelved windows go to a
  shelf band at the bottom of the output they were last on.
- **Units first.** At `Zoom::Units` each unit is one region; inside a region its windows are
  cards. Regions are sized by `weight = 1 + ln(n+1) + 1.5·active + 0.5·explicit_group`, then
  packed by a stable row packer that keeps unit order (stable, not squarified, so chips and
  regions agree). `lege-treemap`'s squarifier is *not* used at the top level, for the same
  reason it is not used on Home: a small weight change should not move every region.
- **Frozen.** The layout is computed on entry with a `generation`; relayout only on: filter
  change that removes or adds units, a window closing, `Zoom` change, or `taskfield.regroup`.
  Query filtering *dims* non-matches rather than relayouting until a whole unit is empty.
- **Cards never exceed the window's real size** and keep aspect; a card is never smaller than
  a legible floor (`MIN_CARD` 160×90 logical), and a unit with more cards than fit at the
  floor shows the first `k` by recency plus a `+n more` card that zooms into the unit.

### 5.4 Semantic zoom

Exactly the ladder Home uses, applied to tasks:

| Zoom | Shows | Enter | Back |
|---|---|---|---|
| `Units` | one region per task unit: name, accent, count, the active window's card large, others small | `Super+Tab`, corner | — |
| `Unit(key)` | that unit's windows as equal cards, `+n more` paged | click a region's header, `Enter` on a region, fan overflow, launcher `→` | `Escape` / `Backspace` |
| `Window(id)` | one window near full size with its transients | `Enter` on a card | `Escape` |

At `Units`, a unit with one window is just that window's card. `Zoom::Unit` for an explicit
group shows the group's cards *in their real relative arrangement* scaled down, because a group
is a spatial thing the user made.

### 5.5 Filters and search

`TaskFilter` comes from whoever opened the mode: `All` from the chord, `Unit(app)` from a fan
overflow or the launcher, `Windows(ids)` from a HomeGarden place (§10), `Shelved` from the
shelf band's header. Typing while in the mode edits `query`; matching is a pure function over
title, application name and group name (`taskfield::matches`) so it works with the shell dead;
the compositor draws the query in its label band and the shell, when present, draws a proper
field. `Enter` activates the selected card; arrows move selection in reading order across the
frozen layout; `Escape` clears the query first, then leaves.

### 5.6 Input inside the mode

The compositor owns hit-testing: pointer over a card selects; click activates and leaves;
right-click on a card or region opens the compositor's own minimal menu (close, park, ungroup)
or, with the shell present, the shell's context menu with the full manifest. Drag of a card
onto another region groups them (`window_groups::group_windows`, the same route as a tag
drop). Keyboard: the chords above; `Delete` closes the selected window (it asks the client;
nothing is killed). Every action is a declared manifest action (`taskfield.*`), so the cheat
sheet and `lege` CLI see it (`check-actions`).

### 5.7 Compositor draws the floor; the shell draws the rest

| Element | Compositor (always) | Shell (when alive) |
|---|---|---|
| Window surfaces at presentation rects | yes | — |
| Region frames, group accents, selection ring | yes (`chrome::collect_taskfield`, evolved from `collect_scatter`) | — |
| Labels | title, count, query — the current scatter label band | title with icon, group name, place name, richer typography |
| Search field, place header, "+n more", shelf header | minimal text | proper widgets on a `Layer::Presentation` surface |
| Context menu | three rows | the manifest menu |

The shell's Taskfield surface is set with `lege_shell_v1.set_layer_surface(Presentation)`; the
compositor maps it only while `Presentation::Taskfield` and routes to it only the events inside
its own rect. If the shell dies mid-mode, the mode continues with compositor labels. This is
what makes Taskfield the successor to the scatter rather than another thing beside it.

### 5.8 Previews and the shell dead

The review's Phase 6 asks for a damage-driven compositor thumbnail cache so the overview never
fails because Home was not visible. Two facts change that here: hidden windows already preview
(`screencopy.rs`), and Taskfield draws surfaces directly. What remains is one honest gap: a
client that has not committed since it was shelved, or a stalled client, shows its last
buffer, which Smithay retains — so nothing extra is needed for correctness. The one addition is
for the **shell's** fan and Home tiles: the compositor keeps the last delivered preview per
window and re-sends it on shell reconnect, so a restarted shell has cards immediately rather
than after its first capture round. Damage-driven, rate-limited at the existing
`MIN_FRAME_INTERVAL`, dropped when the window closes.

### 5.9 Outputs, XWayland, groups

Multi-output: each output shows its own windows; `Super+Tab` opens the mode on all outputs at
once; selection is one global value. XWayland fullscreen: same path — the compositor draws its
surface scaled; the X11 client is told nothing. An explicit group whose members span two
outputs draws as one region on the output holding its anchor, with the far members' cards
marked with the other output's name.

---

## 6. Peek — the pointer's way in, and the end of the hidden bar

Today the bar is dropped while a window is fullscreen and there is no pointer route to anything.
The review's insight is that the answer is not a hiding bar but a staged transition into the
same navigation mode the keyboard uses.

| Stage | Trigger | What the compositor shows |
|---|---|---|
| 0 — fullscreen | — | Nothing. A 2-logical-pixel affordance in the bottom-left corner in the tag colour, drawn by the compositor (it must survive the shell), visible only under fullscreen or a maximized window on an output with no zone. |
| 1 — Peek | pointer dwells in the corner `PEEK_DWELL_MS` (400, the bar's `HOVER_OPEN_MS`) | The current window scales to 94% in place (the first frames of the Taskfield pull-back), and the island appears beneath it, chips only. Nothing else. Moving the pointer away for `HOVER_CLOSE_MS` returns to Stage 0. |
| 2 — Taskfield | from Peek: pointer moves up past the island, or dwells again, or clicks the island's Taskfield chip | `Presentation::Taskfield(All)` |

Keyboard: `Super+Tab` goes straight to Stage 2. The pointer path and the keyboard path are one
system with one state machine; there is no separate "revealed bar under fullscreen" code path,
which is the thing to delete rather than finish.

Peek is also how the island appears over a *maximized* window that has claimed no zone; over
floating windows the island is simply present (§7.6). Touch and touchpad gestures (a
three-finger swipe up into Taskfield) are a later addition to the same state machine, not a
new one.

---

## 7. The Task Island — the bar, kept small

### 7.1 One chip per task unit

A chip is one `TaskUnit`: an explicit group (its accent, its name) or an application with its
loose windows (its icon). Never one chip per window; never a title on the bar; never a folder,
project, launcher result or notification. The active unit gets a subtle emphasis — a 2-pixel
underline in the tag colour, not a 220-pixel title cell (`ACTIVE_WIDTH` goes).

### 7.2 A count, not a row

A chip with more than one visible window shows a small count. That is the whole of the bar's
answer to "how many"; the answer to "which" is the fan or Taskfield.

### 7.3 Direct, fan, Taskfield

`EntryPresentation` (§4) decides on click and on dwell:

- **Direct** (0–1 visible windows): click activates; a unit whose only window is shelved
  unshelves and activates it.
- **Fan** (2–4): the bar surface grows by `FAN_BAND` (replacing `ROW_BAND`) and the windows
  rise out of the chip as an overlapping fan of preview cards, most recent on top, offset by
  24 logical pixels each; hovering a card lifts it and shows its title; click activates. The
  fan reuses the row's dwell/grace state machine and the existing preview path
  (`previews.rs`). Reason for a fan over a row: it is compact, it says "there is a stack
  here", and it has a natural ceiling.
- **Taskfield** (5+): dwell shows the count and a "show all" hint; click opens
  `Presentation::Taskfield(Unit(key))` through `lege_shell_v1.open_taskfield`.

### 7.4 Stable order

Chips keep the order `TaskModel::units` gives: groups first in group-id order, applications in
first-seen order, gaps closed. Activating never reorders. Reason: the bar is spatial memory too.

### 7.5 Parked windows on the island

A unit with shelved windows shows its chip dimmed in proportion (all shelved: fully dimmed;
some: a dimmed segment of the count). Click on a fully-shelved chip unshelves its most recent
window. The full shelf lives in Taskfield's shelf band (§5.3). Home's automatic
`ParkedSession` tiles go (§10.3). This is the `home-taskbar-migration` acceptance.

### 7.6 Where the island is

| Context | Island |
|---|---|
| Home visible, no windows | machine chips only (no bar with nothing on it — kept) |
| Home visible, windows exist | chips |
| Floating windows | chips, on `Layer::ShellOverlay` with the `BAR_HEIGHT` zone, as now; a setting `shell.island.hide-behind-windows` lets it hide until Peek |
| Maximized window on this output | full width: chips left, the window's controls right (Lane A/B, unchanged) |
| Fullscreen window | nothing; the corner affordance; Peek |

### 7.7 Machine chips are not task chips

Network, clock, and — per the HomeGarden plan §11, once decided — battery, volume, brightness
and the power glyph sit in a visually separate cluster at the right end, with a wider gap and
no accent, so the island reads as *tasks | machine*. Ownership does not change: each is a view
of a service (`services-not-indicators`).

---

## 8. Alt+Tab and Alt+` — the small switcher

`Alt+Tab` is **recent windows**, `TaskModel::recent(current, 7)`, newest first, across every
unit and output, visible windows only, a compositor-drawn HUD of up to seven cards (the current
`SwitcherState` HUD with previews when the shell is present, titles otherwise). Holding `Alt`
and pressing `Tab` again steps; releasing activates. Pressing `Super` while holding `Alt`
promotes to Taskfield with the selection carried over — "if you keep holding the modifier or
press a separate shortcut, Taskfield handles the comprehensive case".

`` Alt+` `` is **within the current task unit**, `TaskModel::within_unit(current)`: the
group's members when the focused window is grouped, the application's windows otherwise. This
is the existing `cycle_within_group` semantics under a chord that reads as "this application",
and it costs nothing until a group exists.

No search, no launcher, no forty-window list. Alt+Tab's job is *go back to something I was
just using*.

---

## 9. The launcher — unchanged object, integrated behaviour

The Apps bar on Home stays the one launcher (decided). Three additions, all in the shell's
`omnibox.rs`/`app_search.rs`, reading `TaskModel`:

| Key on a result | Not running | Running |
|---|---|---|
| `Enter` | launch | activate the application's most recent window (`activation_seq`) |
| `Ctrl+Enter` | launch | launch a new instance/window |
| `→` / `Tab` | — | open `Taskfield(Unit(app))` |

Results show a running count (`Firefox · 8 running`). `Super+Space` reveals Home with the bar
focused (§3), so the keyboard route from a fullscreen window to launching something is
`Super+Space`, type, `Enter` — three steps, and the fullscreen window is not disturbed
(`reveal_home` hides, it does not unfullscreen).

---

## 10. The HomeGarden bridge

### 10.1 A place may know its windows

The implemented garden records `LaunchedHere` with an `app_id`. Extend the ledger's context
record with the **instance ids** that were launched at or opened a file under the place
(`HomeActivity.instance_id`, from `Launcher1.Launch`'s returned `instance_id` and from the
brokered-open path, which already has `result.app_id`). A window is associated with a place
when `ShellToplevel.instance_id` is in that set — strong evidence only: launched from the
place, opened a file under it, or explicitly associated (`place.adopt-window`, a drag of a
window card onto a Place in Taskfield or a chip onto a tile on Home). Never "Firefox was open
while you edited something".

### 10.2 "N windows active" → filtered Taskfield

An Expanded place whose associated windows are visible shows one derived line: `4 windows
active`. Pressing it does **not** expand cards inside Home; it opens
`Presentation::Taskfield(Windows(ids))` with the place's name in the header. Home asks *what
work exists*; Taskfield asks *which live windows belong to it*. The shell computes the id list
and sends it over `open_taskfield`; the compositor never learns what a place is.

### 10.3 What leaves Home

`merge_sessions` stops placing a `ParkedSession` tile for every shelved window. A parked window
appears on Home only when it was parked *into* a project or place tile (an explicit
relationship the drop grammar already names: "running window → project = park session in
project"), and there it stays a tile, restored from there. Everything else parked is on the
island (dimmed) and in Taskfield's shelf band. `TileKind::WindowGroup` stays for a group parked
into Home explicitly.

---

## 11. Cohesion — the sense of moving through one system

Additions beyond the review, in scope of the recorded vision, each of which makes two surfaces
feel like one:

1. **One spatial grammar: things shrink in place and grow back to where they were.** Home's
   tile expansion (WS3), Taskfield's pull-back, Peek's 94%, the fan rising out of a chip, a
   window parking into a tile and unparking out of it — all are the same interpolation between
   a real rect and a presentation rect, in `lege_window::geometry::interpolate_rect`, honouring
   `shell.home-field.reduced-motion`. Nothing ever appears from nowhere.
2. **One chip object.** The window's name tag (compositor), the island chip (shell), the
   Taskfield region header (both), the fan card's title strip and a parked tile's edge on
   Home are one design: `TAG_HEIGHT` tall, the same accent, the same typography, the same icon
   rules. `bar_screen.rs` already states this ("the same object on the bar as on the
   window"); extend it to the other three.
3. **One accent per group, everywhere** — already true for tag, outline, chip, tile; add the
   Taskfield region and the fan.
4. **One recency.** `activation_seq` orders Alt+Tab, the fan, the launcher's Enter, a chip's
   unshelve target, and a place's "most recent window". Nothing else invents an order.
5. **One presentation state.** §5.1 collapses reveal, show-all, switcher, lock, recovery and
   Taskfield into one enum with one fullscreen rule and one `space_sync` consequence, so a
   new mode can never be added without answering how it relates to the others.
6. **One route to the shell-dead floor.** The recovery scatter, the fallback switcher HUD and
   Taskfield's compositor rendering are one code path (`chrome::collect_taskfield`), so the
   recovery route is tested every day.
7. **The chords read as a sentence.** `Super` = Home. `Super+Space` = start. `Super+Tab` =
   everything. `Alt+Tab` = recent. `` Alt+` `` = this one. `Super+grave` = terminal. `Ctrl+Alt+…`
   = recovery. Every one is a manifest declaration with a pointer route (`check-actions`).
8. **Idle and pressure.** Taskfield is input (`note_activity`); a throttled or checkpointed
   instance (`resourced`) shows that state on its card, its chip and its Home tile from the one
   `Resources1` hint — the exceptional-only promotion the 2026-09-01 review §13 asks for.
9. **Shell restart is a page reload.** All state that matters — presentation, layout
   generation, selection, MRU, groups, shelf — is in `lege-window`; the shell holds previews
   and chrome only. A shell crash inside Taskfield changes the labels, not the mode.

---

## 12. Where the code goes

| Location | Change |
|---|---|
| `crates/lege-window/src/task.rs` (new) | `TaskModel`, `TaskWindow`, `TaskUnit`, `presentation`, `recent`, `within_unit`; sequences on `WindowRecord`; `WindowService::task_model()` |
| `crates/lege-window/src/taskfield.rs` (new) | `Presentation`, `TaskfieldState`, `layout`, `matches`, zoom/filter transitions, the weighted stable packer; proptests: frozen layout, in-place per output, floor sizes, every card inside its output, total transitions |
| `crates/lege-window/src/service.rs` | `presentation: Presentation` replaces `home_revealed`; `is_visible`/`home_is_covered` consult it; `activate` bumps `activation_seq`; `shelve` records `shelf_rank`; `cycle_focus`/`cycle_within_group`/`cycle_groups`/`group_cycle_targets` replaced by `recent`/`within_unit` |
| `crates/lege-window/src/geometry.rs` | `interpolate_rect`; `scatter_layout` retired once `taskfield::layout` covers the no-shell case |
| `crates/lege-window/src/shell.rs` | `ShellToplevel` + `activation_seq`, `mapped_seq`, `urgent`, `transient_for`, `output`, `shelf_rank` |
| `compositor/lege-comp/src/state.rs` | `show_all_windows`, `switcher` fields fold into the service's `Presentation` |
| `compositor/lege-comp/src/scene.rs`, `chrome/` | `collect_taskfield` (from `collect_scatter`): surfaces at presentation rects, regions, labels, query; the Peek corner affordance; `Layer::Presentation` placement |
| `compositor/lege-comp/src/input.rs` | Taskfield hit-testing and keys; corner dwell → Peek; Alt-hold tracking for Alt+Tab; `Super` during Alt promotes |
| `compositor/lege-comp/src/keybindings.rs` | `Super+Tab` → Taskfield; `Alt+Tab` → recent; `` Alt+` `` → within unit; `window.show-all` → Taskfield compositor-only; `Super+Space` → reveal with bar focus |
| `compositor/COMPOSITOR.md` | Property 5 gains Taskfield; a new row per transition; the fullscreen rule restated as §5.1's invariant |
| `interfaces/wayland/lege-shell-v1.xml` | manager v6: `open_taskfield(filter, ids[])`, `taskfield_state` event (zoom, filter, query, selected), `set_layer_surface` accepts `presentation`; toplevel v4: the five new events; `taskfield_query` event |
| `interfaces/dbus/org.legeos.HomeGarden1.xml` | `Observe` gains `instance_id`; `Contexts` JSON gains `instances` |
| `crates/lege-types/src/action.rs` | `taskfield.open`, `taskfield.zoom-unit`, `taskfield.back`, `taskfield.close-window`, `taskfield.regroup`, `island.show-all`, `window.recent`, `window.within-unit`, `launcher.activate-recent`, `launcher.launch-new`, `place.adopt-window`, `place.show-windows` |
| `shell/lege-shell/src/taskbar.rs`, `dock.rs`, `field_model.rs` | projections of `TaskModel`; `merge_sessions` only for explicit parks |
| `shell/lege-shell/src/surface/bar_screen.rs` | chips without titles; count; fan replaces row; dimmed shelved segment; machine cluster |
| `shell/lege-shell/src/surface/taskfield_screen.rs` (new) | the rich chrome surface |
| `shell/lege-shell/src/omnibox.rs`, `app_search.rs` | running counts, Enter/Ctrl+Enter/→ |
| `crates/lege-session-core/src/settings.rs` | `shell.island.hide-behind-windows`, `shell.taskfield.fan-max` (4), `shell.taskfield.recent-max` (7), `shell.taskfield.peek-dwell-ms` |
| `docs/architecture/task-system.md` (new); `docs/planning/README.md` | the readable form |

---

## 13. Phases and acceptance

Each phase is one AKR work item under a new umbrella `@legeos.work.task-system/1`, with the
decision `@legeos.decision.task-system-four-scales-one-model` recorded first. The order is
the review's, with two phases merged because the substrate already exists.

**Phase 0 — Decisions.** Record the decision; `knowledge.revise` the chord rule of
`window-groups-are-explicit-and-compositor-owned`; note the `Super+Space` route on
`the-home-field-is-a-launcher-not-a-search`; register the source.
*Acceptance:* `decision-recorded` (observation); `validate-clean` (command).

**Phase 1 — TaskModel** (`task-model`). `task.rs`; sequences on records; `ShellToplevel` v4;
`taskbar`/`dock`/`field_model`/the compositor's cycle targets rewritten as projections; MRU.
*Acceptance:* `one-model` (command: a repository gate asserts no other crate computes a
grouping key or window order — grep-based, like `deps-check`); `recency-not-stacking`
(command: proptest that reordering the stack does not change `recent`); `unverified-never-merges`
(command); `protocol-additive` (command: `check-interface-signatures`).

**Phase 2 — Taskfield, compositor, ugly** (`taskfield-mode`). `Presentation`; `taskfield.rs`
layout; surfaces at presentation rects; `Super+Tab` in and out; click/Enter activates; works
from fullscreen, Wayland and XWayland, with the shell dead. No grouping chrome, no search.
*Acceptance:* `from-fullscreen-every-time` (command: nested-session script enters and leaves
Taskfield 100 times over a fullscreen `weston-terminal` and an XWayland `xterm`, asserting
the scene and focus restore byte-for-byte via `--print`); `no-configure-sent` (command: a
protocol trace shows no `xdg_toplevel.configure` during the mode); `shell-dead`
(command: kill the shell mid-mode; activation still works); `layout-frozen` (command: proptest).

**Phase 3 — Units, zoom, filter, search** (`taskfield-semantics`). Regions by unit, weighted
packer, `Zoom`, `TaskFilter`, query matching, the shell's `Layer::Presentation` surface,
context menus, drag-to-group.
*Acceptance:* `zoom-is-total` (command: every transition in §5.4 reaches and returns);
`search-works-shell-dead` (command); `shell-chrome-optional` (command: renders with and
without the shell surface are both accepted baselines).

**Phase 4 — The island** (`task-island`). Chips without titles, counts, stable order, fan,
5+ hands to Taskfield, shelved dimming, machine cluster; delete the row.
*Acceptance:* `no-row-anywhere` (command: `ROW_BAND` gone); `fan-caps-at-four` (command);
`chip-order-stable-under-activation` (command: proptest); `island-hands-on` (manual).

**Phase 5 — Peek** (`peek`). Corner affordance, dwell, 94% pull-back, island under it,
promotion to Taskfield; the fullscreen bar-drop code path deleted.
*Acceptance:* `one-state-machine` (command: no `fullscreen && bar` branch remains);
`peek-under-xwayland-fullscreen` (command); `peek-hands-on` (manual).

**Phase 6 — Previews on reconnect** (`preview-cache`). Last delivered preview re-sent to a
reconnecting shell; damage-driven, rate-limited.
*Acceptance:* `cards-on-restart` (command: shell restart shows fan cards before its first
capture round); `no-idle-wakeups` (command: `idle-wakeups.sh` unchanged).

**Phase 7 — HomeGarden bridge** (`homegarden-windows`). `instance_id` in `Observe`; window
association; `N windows active`; `Windows(ids)` filter; explicit-only parked tiles on Home.
*Acceptance:* `strong-evidence-only` (command: a window with no launch/open link to a place is
never associated); `home-lists-no-windows` (command: `merge_sessions` places nothing
automatic); `home-taskbar-migration` (manual — closes the open acceptance on
`home-project-tool-actions`).

**Phase 8 — Launcher integration** (`launcher-tasks`). Counts, Enter/Ctrl+Enter/→,
`Super+Space`.
*Acceptance:* `enter-activates-recent` (command); `one-launcher-object` (command: no second
launcher surface exists).

**Phase 9 — Stress** (`task-system-stress`). Scripted nested sessions with 1, 5, 20, 50 and
100 windows; one application owning 50; 20 applications; fullscreen Wayland and XWayland;
laptop + HDMI and the hotplug fixture; shell crash and restart inside every mode.
*Acceptance:* `frame-budget` (command: Taskfield entry under 16 ms at 100 windows on the
reference laptop, measured like `home-refresh-costs-profiled`); `nothing-lost`
(command: after every scenario, every window is reachable by Taskfield and `is_recoverable`
holds); `stress-hands-on` (manual).

Dependencies: Lane A and Lane B hands-on confirmations (open); WG5 hands-on (open); the
HomeGarden implementation (done); the resourced hint path for §11.8 (exists).

---

## 14. Open questions for the user

1. **Alt+Tab cap.** Seven recent windows, or five? The HUD's width decides.
2. **Peek corner.** Bottom-left (beside where the island starts) or bottom-right (where a
   maximized window's controls are)? The plan says bottom-left so Peek reveals the island in
   place.
3. **Island under floating windows.** Always present (as now), or hidden until Peek by
   default? The plan keeps it present and adds the setting.
4. **Squarified or stable regions** at `Zoom::Units`? The plan chooses the stable row packer
   so chips and regions share an order; the treemap is prettier and moves more.
5. **Explicit park into a place** — is a chip dropped on a Home tile enough of a gesture, or
   should it require the drop grammar's named operation preview?
6. **`window.show-all` chord.** Keep `Ctrl+Alt+Space` as the recovery spelling of Taskfield,
   or retire it now that `Super+Tab` is compositor-owned? The plan keeps it: a recovery chord
   should not depend on a logo key.
7. **Touch.** Three-finger swipe up into Taskfield is cheap once the state machine exists;
   include in Phase 5 or defer?

---

## Appendix A — Chords, before and after

| Intent | Before | After |
|---|---|---|
| Home | `Super` tap, `Ctrl+Alt+H`, `Super+D` | unchanged |
| Start something | click the Apps bar on Home | + `Super+Space` (reveal with bar focused) |
| Recent window | — | `Alt+Tab` (≤7, MRU) |
| Within this app / group | `Alt+Tab` (within group) | `` Alt+` `` |
| Across groups | `Super+Tab` | subsumed by Taskfield's `Units` level |
| Every window | `Ctrl+Alt+Space` (cards) | `Super+Tab` (Taskfield); `Ctrl+Alt+Space` = the same, compositor-only |
| Mouse under fullscreen | none | corner → Peek → Taskfield |
| Park / terminal / group / step / escape | `Ctrl+Alt+P`, `Super+grave`, `Super+G`, `Super+←/→`, `Ctrl+Alt+Esc` | unchanged |

## Appendix B — Two walkthroughs

**Thirty windows, one fullscreen.** Firefox is fullscreen with 14 windows; six terminals, three
Files windows, Blender, and the *Save Your Skin* group of four are behind it. `Super+Tab`: the
Firefox surface shrinks in place to a card inside a *Firefox · 14* region; five other regions
appear around it, sized by log(n) with the group drawn in its accent as one region with four
cards in their real arrangement; the terminals' region shows three cards and *+3 more*. Typing
`smith` dims everything but two Firefox cards. `Enter` activates *Smithay docs*; the scene
expands back; Firefox is no longer the activated window but is still fullscreen behind. `Alt+Tab`
later shows *Smithay docs*, *cargo test*, *Firefox — GitHub*… as seven cards.

**A place's windows.** On Home, the `legeos` place (Expanded) shows `4 windows active`.
Pressing it opens Taskfield with the header *legeos — 4 windows*: the kitty launched from its
terminal action, the editor opened on `service.rs`, the Files window on `crates/`, and the
Firefox window that opened `docs/architecture/task-system.md` through the brokered route. The
YouTube Firefox window is not there. Press `Escape` twice: Home again, nothing moved.

---additional context----

Yes. The distinction needs to be made much sharper, or it risks feeling like TemplarOS has **two competing desktops**.

I would define it this way:

> **HomeGarden is a place. Taskfield is a view.**

That should become an architectural and UX rule.

HomeGarden is the actual persistent desktop environment. It exists even when nothing is running. Projects, folders, sessions, places, and contextual information live there. The user can enter it, remain there, manipulate things there, and launch work from it.

Taskfield does **not** exist as a place. It is a temporary compositor projection of the windows that already exist.

## The distinction

|                           | HomeGarden                   | Taskfield                         |
| ------------------------- | ---------------------------- | --------------------------------- |
| Concept                   | **Place / desktop**          | **Window overview / mode**        |
| Persistent                | Yes                          | No                                |
| Exists with zero windows  | Yes                          | Essentially no                    |
| Contains files/projects   | Yes                          | Never                             |
| Contains live windows     | Only references/counts       | Yes                               |
| Launch applications       | Yes, contextually            | No                                |
| Arrange persistent things | Yes                          | No                                |
| Rearrange windows         | Maybe via WM actions         | Only indirectly                   |
| Stay there indefinitely   | Yes                          | No                                |
| Activation                | Super / Home                 | Super+Tab                         |
| Exit                      | Open something or leave Home | Select window / Esc               |
| Remembers layout          | Yes                          | Only enough for stable navigation |
| Semantic organization     | Projects/places              | Running application/group/window  |

That last distinction is especially important:

**HomeGarden organizes intentions. Taskfield organizes consequences.**

You started a Save Your Skin development session from HomeGarden. The editor, terminal, browser, and file manager that resulted from that work happen to be visible in Taskfield.

---

## Taskfield shouldn't look like another desktop

I would revise some of my previous presentation accordingly.

I would **not** let the user freely arrange Taskfield cards spatially like desktop objects.

Don't let this happen:

```text
TASKFIELD

 [Firefox]          [Terminal]

             [Files]

 [Blender]                    [Code]
```

if those positions acquire permanent meaning.

Then you've accidentally created Desktop 2.

Instead, Taskfield should obviously be a generated view:

```text
              WINDOWS

  RECENT

  ┌───────────────┐  ┌───────────────┐
  │     Code      │  │   Terminal    │
  │               │  │               │
  └───────────────┘  └───────────────┘

  SAVE YOUR SKIN · 4
  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐

  FIREFOX · 7
  ┌─────────┐ ┌─────────┐ ┌─────────┐   +4

                 Search windows…
```

It's much closer to **Exposé/Mission Control**, except with Templar's application/group semantics and better scaling.

The compositor calculates this arrangement every time Taskfield opens. It may preserve positions during that invocation so things don't jump around, but when you leave, **Taskfield ceases to exist**.

That's an important psychological distinction.

---

# HomeGarden should actually be underneath

There's a neat relationship available here because you're writing your own compositor.

Consider the normal conceptual stack:

```text
        SYSTEM MODES
        Taskfield
        Alt-Tab
        Launcher
            ↑
────────────────────────
        LIVE WORK
        Windows
            ↑
────────────────────────
        HOME
        HomeGarden
```

HomeGarden is the bottom level.

Applications emerge from it.

Taskfield temporarily steps above the applications to let you inspect them.

So the navigation has a logical direction:

```text
          TASKFIELD
       "show my windows"
              ↑
              │
           WINDOWS
       "doing my work"
              ↑
              │
         HOMEGARDEN
       "my work exists"
```

That is substantially different from conventional virtual desktops.

You are not moving sideways between:

```text
Desktop 1 ↔ Desktop 2
```

You're moving vertically through levels of abstraction.

I think that is a strong model for TemplarOS.

---

# A concrete example

Suppose HomeGarden contains:

```text
HOMEGARDEN

             Save Your Skin
             ├ source
             ├ docs
             ├ assets
             └ 4 running

      Lege                       JPEG XL
      └ 1 running                └ 3 running

                  Downloads
```

You select Save Your Skin and begin working.

Now normal desktop operation is:

```text
┌──────────────────────────────────────────────────────┐
│                                                      │
│                        CODE                          │
│                                                      │
│                                                      │
│                                                      │
│                                                      │
│                [task island]                         │
└──────────────────────────────────────────────────────┘
```

You want another existing window.

`Super+Tab`.

The current scene pulls back:

```text
TASKFIELD

SAVE YOUR SKIN
┌────────────────┐ ┌──────────────┐ ┌──────────────┐
│                │ │              │ │              │
│      Code      │ │   Terminal   │ │    Browser   │
│                │ │              │ │              │
└────────────────┘ └──────────────┘ └──────────────┘

OTHER
┌──────────────┐   ┌──────────────┐
│    Music     │   │   Downloads  │
└──────────────┘   └──────────────┘
```

Click Terminal.

Taskfield vanishes.

You're now looking at Terminal.

It should feel no more like having entered another desktop than Alt-Tab does.

It's just a much richer Alt-Tab.

---

# Going Home is fundamentally different

Pressing `Super` should produce a very different transition.

Instead of the **current windows pulling back for inspection**, the windows get out of the way and you return to the persistent HomeGarden:

```text
HOMEGARDEN

     ┌──────────────────────┐
     │  Save Your Skin      │
     │                      │
     │  source       docs   │
     │  assets              │
     │                      │
     │  ● 4 windows active  │
     └──────────────────────┘

           ┌────────────┐
           │    Lege    │
           └────────────┘
```

That transition should communicate:

> I'm leaving the immediate window layer and returning Home.

Whereas Taskfield communicates:

> I'm momentarily looking at my open work from above.

Those animations can reinforce the architecture tremendously.

---

# I'd even give them opposite visual motion

This can prevent confusion without adding explanatory UI.

### HomeGarden

Windows **move away/recede**, exposing Home underneath.

```text
windows ↑ disappear
────────────────────
HomeGarden remains
```

It feels like uncovering something persistent.

### Taskfield

Windows **remain the subject** and transform into cards.

```text
fullscreen window
      ↓ shrinks
window cards appear around it
```

Nothing underneath is revealed.

It's just the current window world changing presentation.

This gives the user a physical intuition for what each mode means.

---

# Don't show HomeGarden inside Taskfield

I would avoid putting a Home tile in the Taskfield grid.

Earlier I suggested possibly exposing Home through Peek. I'd now narrow that.

A dedicated Home affordance can exist in the task island or keyboard navigation, but **HomeGarden itself shouldn't be another card among windows**:

```text
BAD

[Firefox] [Terminal] [HOME] [Code]
```

That implies equivalence.

Instead:

```text
Esc             return
Super           Home
Super+Space     launch
```

or a subtle fixed system affordance outside the actual window field.

Home is structurally below the field, not one of its contents.

---

# Nor should normal HomeGarden cards display live window previews

This is another place where they could merge accidentally.

I'd keep:

```text
Save Your Skin
4 running
```

rather than:

```text
Save Your Skin
[Code preview] [Terminal preview] [Firefox preview]
```

The latter turns HomeGarden into another window manager.

You can click:

**4 running → open Taskfield filtered to Save Your Skin**

That's the bridge.

So:

```text
HOMEGARDEN
Save Your Skin
"4 running"
        │
        ↓
TASKFIELD
filter: Save Your Skin
        │
        ↓
Terminal
```

And the reverse:

```text
TASKFIELD
Save Your Skin group
        │
  "show in Home"
        ↓
HOMEGARDEN
Save Your Skin
```

The relationship is navigable without collapsing their purposes.

---

# The semantic hierarchy should probably be explicit in the model

I'd represent it roughly like this:

```rust
pub struct Place {
    id: PlaceId,
    // Persistent semantic object.
}

pub struct Window {
    id: WindowId,
    app: AppId,

    // Optional relationship to persistent context.
    place: Option<PlaceId>,
}
```

Taskfield does not own objects:

```rust
pub struct Taskfield {
    // Projection only.
    visible_windows: Vec<WindowId>,
    grouping: GroupingMode,
    query: String,
}
```

There shouldn't be a:

```rust
taskfield.save_layout()
```

or:

```rust
window.taskfield_position
```

unless that information is exclusively ephemeral.

That's a useful architecture test.

If you're storing permanent Taskfield geography, it is probably turning into another desktop.

---

# I'd also reconsider the name eventually

**Taskfield** is useful internally while designing it, but "field" sounds enough like a workspace that it may contribute to exactly the ambiguity you're noticing.

Potentially the user-facing feature could simply be called:

* **Windows**
* **Overview**
* **Window Overview**

while `Taskfield` remains the internal component name.

For example:

```text
Super       Home
Super+Tab   Windows
Super+Space Open
```

That's remarkably understandable.

Templar can have novel mechanics without assigning novel vocabulary to every mechanic.

I particularly like:

> **Home** = HomeGarden
> **Windows** = Taskfield
> **Open** = launcher

Then someone can understand the shell in ten seconds.

---

## The resulting desktop model

I would formalize TemplarOS as having **one desktop and three temporary system views**:

```text
                    TEMPORARY VIEWS

          Open          Windows          Switch
        launcher       Taskfield        Alt-Tab
           ↑               ↑               ↑
           └───────────────┼───────────────┘
                           │
                    RUNNING WINDOWS
                           │
                           ↓
                    ───────────────
                           │
                         HOME
                      HomeGarden
                           │
             projects / places / files
```

So yes, your reading was close:

> HomeGarden is where projects and persistent working contexts can be interacted with. Taskfield is where all currently running windows can be seen and selected.

But I would **not** say that the windows "live" or are "arranged" in Taskfield.

They **live in the compositor's ordinary window space**.

Taskfield merely *projects them into a temporary navigable arrangement*.

That small conceptual distinction is what prevents TemplarOS from feeling like it has two desktops.
