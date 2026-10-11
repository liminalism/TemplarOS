# The task system

Five scales, each answering one question:

| Scale | Question | Where |
|---|---|---|
| HomeGarden | what am I working on? | the shell's Home Surface — a **place** |
| Task Island | what can I return to? | the bottom rail's live tasks and saved launch targets |
| Taskfield | where are all my windows? | the compositor's window overview — a **view** |
| Launcher | what can I find or open? | one global Apps/Files overlay |
| `Alt+Tab` | which window next? | every open window, most recent first |

**HomeGarden is a place; Taskfield is a view.** Home exists with nothing running: projects, places
and files live there and can be arranged. Taskfield is a temporary projection of windows that
already exist — laid out when it opens, discarded when it closes. It never contains a file, never
launches anything, and no card position is ever stored against a window. The moment one is, it has
become a second desktop. The navigation is vertical, not sideways: Home is below the windows, the
overview steps momentarily above them.

## One model

`templar_window::task::units(&[ShellToplevel]) -> Vec<TaskUnit>` is the only grouping in the
system. A unit is an explicit window group (which outranks application identity) or an application
with its loose windows; a window whose identity is unverified is its own unit and never merges into
a trusted application's chip. Units keep first-seen order — chips must not swim — and recency
orders windows *inside* a unit only.

It is a pure function over `ShellToplevel` rather than a method on the service, because the
compositor holds those records and the shell receives exactly the same ones: both sides run the
same function on the same bytes. Recency is not recomputed anywhere — `ShellToplevel::mru_rank` is
`WindowRecord::last_activated`, bumped in `WindowService::activate`, so `Alt+Tab`, the chip's row,
the launcher's Enter and a chip's unshelve target cannot invent an order of their own.

`ChipAction` decides what a chip does: a unit of one window activates it (unparking it first); a
unit of two or more opens a **row** of separate cards above the chip — on-screen windows first,
then parked ones, at most `FAN_MAX` — unless more than `FAN_MAX` are on screen, which hands over to
Taskfield. Parked windows count toward the row because a terminal put away by the drop-down chord
is still one of that terminal's windows; they never push a chip to Taskfield, whose unit filter
shows on-screen windows only. The row's cards sit side by side with a gap, each titled, never over
each other — the earlier overlapping fan hid every card but the top one. The bar is insufficient by
design; a linear strip is intrinsically bad at thirty windows.

A chip is the application's icon, never a word: the name is a hover tooltip. A group, which is more
than one application, wears its most recently activated member's icon beside its accent stripe, and each
row card falls back to its own window's icon until a capture arrives. An initial is drawn only when
no icon resolves at all — the lookup tries the desktop entry's `Icon=`, then the desktop file's own
name, and is repeated with the manifest's hints once the application catalogue arrives.

Persisted favorites are a separate input to rail reconciliation, not stored `TaskKey`s.
`rail_model.rs` combines them with the taskbar projection before drawing: a saved plain app
and its running task occupy one chip, while named terminal directory/argument profiles keep
their own launch configuration. Reconciliation retains actual task units and first-seen order,
including explicit groups and unverified windows; it does not merge away distinct authoritative
`TaskKey`s. A saved target with no matching window launches through `Launcher1`.

The launcher shares the catalogue, favorite metadata and window activation path. Blank Apps
shows favorites followed by recent applications, suppressing duplicate plain app identities;
typing uses `app_search::search`. Tab retains the query while switching to Index1 Files search.
Click or Enter activates immediately and closes the popup. Ctrl+Enter requests a new instance;
Right shows existing app/profile windows. Shortcut creation is an explicit context action,
never a side effect of choosing a result. See [the launcher guide](../launcher.md).

## Alt+Tab

`Alt+Tab` walks `WindowService::alt_tab_order`: every mapped application window, parked ones
included, most recently activated first — the same windows the island shows, so the two never
disagree about what is open. The list is frozen at the first press; `Tab` only moves the selection,
and releasing the modifier activates it (unparking a parked window). The shell draws the walk —
icons and live previews — on its `Layer::Presentation` surface, which the compositor shows only
while Taskfield or a walk is up (`WindowService::set_switch_walk`); with no visible shell surface
the compositor's own text HUD draws instead.
`Escape`, a click or any other chord cancels the walk without activating anything, and a window that
closes mid-walk leaves the list. `` Alt+` `` is the same walk scoped to the focused window's unit.

## One presentation

`WindowService::presentation()` is `Normal`, `HomeReveal` or `Taskfield`. It carries the invariant
that replaces every fullscreen special case:

> A fullscreen window covers passive chrome only while `Normal`.

Passive chrome (the island, Home, tags) yields to fullscreen; explicit navigation yields to
nothing. `is_visible`, `home_is_covered` and the `stacking_order` exception each consult that one
value. The lock screen and the recovery overlay are not modes here — they are `Layer`s, and a layer
outranks everything by construction.

The explicit launcher is an independently focused `Layer::ShellOverlay` surface. Super+Space
requests it without changing `presentation()` or revealing Home. The compositor lends it
keyboard focus above fullscreen/passive chrome, and dismissing it returns to the underlying
activation. Outside-click dismissal is enforced by compositor focus handling because a
Wayland client cannot observe pointer events sent to another client.

In Taskfield the compositor draws each window's surface tree at
`WindowService::presentation_rect(id)`. No `xdg_toplevel.configure` is sent, clients keep painting
at their real sizes, and leaving is a render change rather than a geometry change — which is why it
works from fullscreen, Wayland and XWayland alike, every time.

## The layout

`templar_window::taskfield::layout` is pure: one section per unit per output (a window does not
jump screens to be found), cards in rows, the largest cells whose total height fits, floored at
`MIN_CARD`. A card keeps its window's aspect and is never enlarged past its real size. What does
not fit is counted as `overflow` and drawn as `+n more`, whose route is the same mode filtered to
that unit — where it does fit. Nothing is unreachable.

A typed query *dims* non-matches (`task::matches`); it is not an input to the layout at all, so
nothing moves under the pointer as the user types.

The shell receives no card rectangles. It runs the same `layout` over the toplevels and outputs it
already holds, so its labels cannot land anywhere but on the cards — and with the shell dead the
compositor draws its own and the mode works exactly as well.

## Chords

`Super` Home · `Super+Space` Apps/Files launcher · `Super+Tab` the overview · `Alt+Tab` next window ·
`` Alt+` `` this one · `Ctrl+Alt+Space` the overview again, as the recovery spelling.

`Super+Tab` used to cycle between window groups; the overview answers that question by showing
every group as a section. Every one is a manifest declaration with a pointer route
(`crates/templar-types/src/action.rs`, `xtask check-actions`).

See `compositor/COMPOSITOR.md` for the compositor's half of the contract,
`interfaces/wayland/templar-shell-v2.xml` for the wire, and
`docs/task-system-taskfield-island-peek.md` for the design this came from.
