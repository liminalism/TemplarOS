# Taskbar lanes

**This is a deliberate feature.** Windows, Ubuntu, macOS and COSMIC group a program's windows
together, but nothing groups *what the programs are for*: four terminals doing work end up
interspersed with folders that are just open. The running-apps strip (the bar's island and the
Apps rail of the gesture bars) keeps like windows contiguous instead — terminals together,
everything else together — and can draw the two groups as two rows.

## What it is not

- **Not `htop` for the taskbar.** Nothing samples CPU, walks `/proc`, watches a process tree, or
  re-sorts by load. An earlier sketch of this feature measured "working or not" per window; it
  was dropped for being far more machinery than the goal needs. Do not add it back without the
  owner asking.
- **Not automatic churn.** A chip changes lane only when its windows open, close, or belong to a
  different application — the same moments the strip already changes. Using a window never moves
  a chip (the chips are spatial memory: `taskbar::tests::using_a_window_never_reorders_the_bar`).

## The rule

An application is on the **active lane** when its desktop entry declares the freedesktop category
`TerminalEmulator` (`taskbar::ACTIVE_LANE_CATEGORY`). Everything else is on the **latent lane**.
The set of active-lane application ids is read from the application catalogue the shell already
holds (`taskbar::active_lane_apps`) and refreshed when the catalogue is.

- An entry is on the active lane if any of its windows belongs to such an application. A window
  group that holds a terminal is therefore active.
- Identity is never taken on a window's word: a window whose identity is unverified has no
  application id and is always latent (`TaskKey::Unknown`).
- "And similar for other kinds of windows" is the extension point: `taskbar::Lane` is an enum and
  the classification is one function. New kinds (an editor lane, a media lane) add a category and
  a rule there; the layout does not change.

## The setting: `shell.rails.lanes`

| Value | Effect |
|---|---|
| `off` | First-seen order, as before this feature. |
| `contiguous` (default) | One row. Active-lane chips first, then the rest; first-seen order inside each lane (a stable partition, `taskbar::entries_in`). |
| `rows` | Two rows. Active-lane chips on the strip's own row, latent chips one step above it (`bar_screen::ROW_STEP`). Each row is centred; the surface grows by one step and no more. |

`rows` falls back to `contiguous` when it cannot be honest (`taskbar::rows_of`): if either lane is
empty (no empty row is drawn), or either lane is longer than one row of the island / rail holds
(`bar_screen::island_row_capacity`, `rail_row_capacity`) — a lane that needs an overflow chip
cannot share a row layout, because an overflow chip stands for the *tail of the list*, not the tail
of a lane. The one-row layout already knows how to overflow.

The setting is a normal `shell.rails.*` key: it appears on the Settings Rails page and is read by
the shell only (the compositor has no part in it — the bar's grouping is presentation).

## Where it lives

| Piece | Where |
|---|---|
| Lane, mode, rows, classification, ordering | `shell/templar-shell/src/taskbar.rs` |
| Two-row geometry, sizes, capacities | `shell/templar-shell/src/surface/bar_screen.rs` (island and rail) |
| Host wiring (`bar_entries`, catalogue refresh) | `shell/templar-shell/src/surface/app.rs` |
| Setting | `shell.rails.lanes` in `crates/templar-session-core/src/settings.rs` |

`templar_window::task::units` — the one task model that `Alt+Tab`, Taskfield and the launcher share
— is deliberately untouched: lanes are how the *bar* lays units out, not a second opinion about what
is running. `Alt+Tab` order stays MRU.

Every consumer of a chip's *index* (draw, hover, fan, deadline) reads `App::bar_entries`, never
`taskbar::entries`, or the index would name a different chip.

## Known limits

- Classification is by category only. An application with no `Categories=` (or a terminal that
  forgot `TerminalEmulator`) sits on the latent lane.
- In `rows`, a chip that changes lane (a terminal opening, say) moves rows immediately; the strip
  does not wait for the pointer to leave it.
