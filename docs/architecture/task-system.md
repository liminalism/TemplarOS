# The task system

Four scales, each answering one question, none duplicating another:

| Scale | Question | Where |
|---|---|---|
| HomeGarden | what am I working on? | the shell's Home Surface — a **place** |
| Task Island | what is running? | the bottom bar, kept small on purpose |
| Taskfield | where are all my windows? | the compositor's window overview — a **view** |
| Apps bar | what can I start? | Home's launcher |
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
`WindowRecord::last_activated`, bumped in `WindowService::activate`, so `Alt+Tab`, the fan, the
launcher's Enter and a chip's unshelve target cannot invent an order of their own.

`ChipAction` decides what a chip does by how many windows are on screen: 0–1 activates, 2–4 fans
out of the chip, more hands over to Taskfield. The bar is insufficient by design; a linear strip is
intrinsically bad at thirty windows.

A chip is the application's icon, never a word: the name is a hover tooltip. A group, which is more
than one application, wears its most recently activated member's icon beside its accent stripe, and each
fan card falls back to its own window's icon until a capture arrives. An initial is drawn only when
no icon resolves at all — the lookup tries the desktop entry's `Icon=`, then the desktop file's own
name, and is repeated with the manifest's hints once the application catalogue arrives.

## Alt+Tab

`Alt+Tab` walks `WindowService::alt_tab_order`: every mapped application window, parked ones
included, most recently activated first — the same windows the island shows, so the two never
disagree about what is open. The list is frozen at the first press; `Tab` only moves the selection
in the compositor's HUD, and releasing the modifier activates it (unparking a parked window).
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

`Super` Home · `Super+Space` start · `Super+Tab` the overview · `Alt+Tab` next window ·
`` Alt+` `` this one · `Ctrl+Alt+Space` the overview again, as the recovery spelling.

`Super+Tab` used to cycle between window groups; the overview answers that question by showing
every group as a section. Every one is a manifest declaration with a pointer route
(`crates/templar-types/src/action.rs`, `xtask check-actions`).

See `compositor/COMPOSITOR.md` for the compositor's half of the contract,
`interfaces/wayland/templar-shell-v1.xml` (manager v8) for the wire, and
`docs/task-system-taskfield-island-peek.md` for the design this came from.
