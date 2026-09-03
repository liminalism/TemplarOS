# Window groups, and the application integration choices

*Planning document, 2026-09-03; revised the same day after the user's reading (§2 rules 6–7,
§2.1, §4.2, §4.6, §6). The authoritative form is the AKR ledger:
`@legeos.decision.window-groups-are-explicit-and-compositor-owned`,
`@legeos.work.window-groups`, `@legeos.decision.the-home-field-is-a-launcher-not-a-search`
and `@legeos.work.application-integration-interim`. This file is the readable version.*

## 1. What the fourth hands-on run found

The user ran the third build set in place and reported, in order of daily cost:

| # | Report | Root cause | Where it is handled |
|---|--------|-----------|---------------------|
| 1 | `sudo lege dev deploy` failed: hostd would not restart, and every later component was "not attempted" | hostd has crash-looped since 2026-09-01 on `EACCES` connecting to the system bus socket; the deploy tool stops at the first failed activation even though every package is installed | Opus agent A: root cause hostd; make activation continue past a failure |
| 2 | Firefox and Chromium: "no Wayland display, access denied" | snapd's AppArmor wayland rule allows only `/run/user/N/wayland-[0-9]*`; the compositor listens on `legeos-0` | Agent A: public socket named `wayland-N` |
| 3 | Titles bleed through a translucent window at full opacity; windows "fight for focus" | every band and grip is rendered as one layer above every window (`backend/udev.rs`); focus behaviour under diagnosis | Opus agent B: interleave chrome per window in stacking order; diagnose focus |
| 4 | No screen timeout | Power1 records an idle policy and nothing enforces it; there is no idle source | Agent B: compositor idle timer, outputs off, Settings control |
| 5 | The search bar finds files for "firefox", never the browser; apps unreachable | sessiond imported desktop entries from four hard-coded directories and never `/var/lib/snapd/desktop/applications`, so the snap browsers were not in the registry; the bar federated files, projects and actions with applications | Fixed this session: `d391fc1` (registry scans every XDG data root and the snap directory), `dfa1fda` (the bar is an application launcher, labelled Apps; file search lives in the Files object) |
| 6 | Kitty as the default terminal, integrated | no terminal role exists in Providers1; the manifest only reserves no chord | §4.3 |
| 7 | The file manager has no right-click menu and needs to earn its keep | the Files object has verbs on a toolbar but no secondary-click route and no multi-file workflow | §4.4 |
| 8 | fsearch for real search | lege-filed indexes roots for scoped search; fsearch 0.2.3 is installed on this host | §4.2 |
| 9 | Window groups | new capability | §2–3 |

Writing text was reported snappier than COSMIC. Nothing in this plan touches the input path.

## 2. Window groups: the decision

A **window group** is an explicit, user-made set of two or more application windows that
behave as one unit of work: they park and return together, they are one tile on Home that is
dragged as one, `Alt+Tab` moves inside the group, and `Super+Tab` moves between groups.

Seven rules, each with the reason it is a rule:

1. **Groups are explicit.** A group exists only because the user made it, by dropping one
   window on another or by a chord. Nothing is grouped by proximity, by application, or by
   time. `docs/homefield.md` already forbids inferred relations ("do not infer hidden meaning
   from proximity") and the existing `Super+Left/Right` spatial step deliberately stores
   nothing; groups are the stored, visible form that rule asks for.
2. **The compositor owns groups, through `lege-window`.** `Alt+Tab` and `Super+Tab` must keep
   working with the shell dead (every reserved chord does), and the Home tile is a
   representation of the group, not its source of truth. So `WindowService` holds membership,
   and the shell learns it over `lege_shell_v1`, exactly as it learns shelving today.
3. **A group is one thing to stack.** Activating any member raises every member together,
   keeping their relative order, the activated one on top. This is what makes a group feel
   like one window set rather than a label; it is also the rule that keeps `is_recoverable`
   intact, since raising changes no geometry.
4. **Ungrouped windows are the loose set.** With no groups, `Alt+Tab` cycles every window as
   it does today (the loose set *is* the current group), and `Super+Tab` does nothing. The
   feature costs nothing until a group exists.
5. **A group has at least two members.** When the second-to-last window leaves or closes,
   the group dissolves. Its Home tile is swept the way an orphaned session tile is.
6. **Grouping places, it never merges.** Dropping one window onto another's title band groups
   them *and* sets the dropped window down beside the target, on the nearest free side, edge
   to edge. Nothing is stacked, tabbed or folded into one frame (the iOS-folder gesture is
   explicitly not wanted). Proximity never *means* similarity — rule 1 stands — but a group,
   once made, is laid out as neighbours, because that is what makes a set of windows readable
   as one piece of work. A plain drag to any other place, including on top of another window,
   moves a window and groups nothing; only the band is a drop target, so ordinary overlapping
   placement is untouched.
7. **A group is visibly one thing.** The compositor draws one outline around the union of the
   members' rectangles, two logical pixels in the group's accent colour, and a stripe of the
   same colour in each member's band; the Home tile takes the same colour for its edge. The
   colour is assigned from a short palette by the group's id and lives with the group in
   `lege-window`, so the workspace, the switcher and Home can never disagree about it. The
   outline is compositor chrome, drawn per window in stacking order like the bands, so it
   survives the shell dying.

### 2.1 The chords, and why `Ctrl+Tab` is not one of them

`Ctrl+Tab` is the tab-cycling chord in Firefox, Chromium, Kitty, VS Code and GNOME Terminal; a
compositor-reserved chord wins, so those applications would lose it. The user has chosen
`Super+Tab` for moving between groups instead, and there is no `Ctrl+Tab` binding and no
setting: one chord, always the same, never conditional on whether a group exists.

| Chord | Does |
|-------|------|
| `Alt+Tab` / `Alt+Shift+Tab` | next / previous window *inside* the focused window's group (the loose set when it is in none) |
| `Super+Tab` / `Super+Shift+Tab` | next / previous *group*, landing on that group's last active member; the loose set counts as one group |
| `Super+G` | group the focused window with the previously focused one (and set it down beside it, rule 6) |
| `Super+Shift+G` | take the focused window out of its group |
| band dropped on a band | group and place (rule 6); the target band highlights while hovered |
| band drag | moves the whole group as one; `Shift` held moves that member alone |

`Super+Left/Right` keep their existing meaning, the stored-nothing spatial step to the next
window. Their actions are renamed from `window.group-next/previous` to
`window.step-right/left` so the word *group* means one thing in the manifest.

## 3. Window groups: the plan

Five work items, in dependency order. Estimates are for one focused agent on this codebase.

### WG1 — the model in `lege-window` (pure, property-tested)

- `GroupId(u64)`; `WindowRecord.group: Option<GroupId>`;
  `WindowService.groups: BTreeMap<GroupId, GroupRecord { name: Option<String>, accent: u32,
  last_active: WindowId }>`.
- `group(a, b) -> GroupId`: joins `b` to `a`'s group; creates one if neither has one; merges
  two groups when both have one (union, `a`'s identity survives). `ungroup(window)`,
  `dissolve(group)`. Dissolution on the last-but-one departure and on destroy.
- `LayerStack::raise_many(ids)` and `activate` raising the whole group contiguously.
- `shelve_group` / `unshelve_group` (every member, one call, one `shelf_rank` for the set so
  the shelf lists the group as a unit), `move_group(anchor, geometry)` moving every member by
  the anchor's displacement, repaired per member; `place_beside(window, target)` choosing the
  nearest free side of `target` (right, left, below, above, in that order of preference when
  equidistant) and setting `window` edge to edge there, repaired to the output.
- `group_outline(group) -> Rect`: the union of the members' rectangles, for the chrome.
- The accent: `GroupRecord.accent` from a six-entry palette indexed by `GroupId`.
- `cycle_within_group(current, forward)`: stacking order filtered to the current window's
  group or the loose set, shelved skipped. `cycle_groups(current, forward)`: the focus target
  of each group is its `last_active` unshelved member; the loose set counts as one group whose
  target is its topmost unshelved window; targets are ordered by stacking order.
- `ShellToplevel` gains `group: Option<GroupId>`; snapshot carries the group table.
- Properties: raising never changes geometry; membership survives shelve, maximize,
  arrangement and output change; every member of a live group is a client window; no window
  is in two groups; a group always has two or more members.
- New `WindowCommand`s: `Group { a, b }`, `Ungroup { window }`, `ActivateGroup { window }`,
  `ShelveGroup { window }`, `UnshelveGroup { window }`, `SetGroupName { window, name }`.
  About 1.5 days including tests.

### WG2 — the compositor

- `RecoveryAction::CycleFocusForward/Backward` become within-group (`Alt+Tab`); new
  `GroupCycleForward/Backward` (`Super+Tab`, `Super+Shift+Tab`); `Super+G` groups the active
  window with the previously active window and sets it down beside it, `Super+Shift+G`
  ungroups it (keyboard route; the context menu gets rows so the chord is never the only
  route, per the action manifest's rule). The existing `GroupNext/Previous` spatial step is
  renamed `StepRight/Left`.
- The fallback switcher HUD lists the group's members under the group name; the group HUD
  lists groups with member counts and the loose set as "Other windows".
- The band shows a 4 px accent stripe and the group name before the title for grouped
  windows, and the compositor draws the group outline (rule 7) around the union of the
  members' rectangles. Drag-and-drop grouping on the workspace: releasing a band drag with
  the pointer over another window's band groups them and places the dropped window beside
  the target (`place_beside`, rule 6); the target band highlights while hovered.
- A band drag moves the whole group (`move_group`); `Shift` held moves that member alone.
- `COMPOSITOR.md`: rows for group stacking, group chords, band drop, settings-driven chord.
- No settings channel: the chords are fixed (§2.1). About 2 days.

### WG3 — `lege_shell_v1`: toplevel version 3, manager version 5

Declared in `interfaces/wayland/lege-shell-v1.xml` at `9d8e3f6`, additions only:

- `lege_shell_toplevel_v1.group` event since 3: `group_id uint` (0 = none), `name string`,
  `accent uint` (0x00RRGGBB).
- `lege_shell_toplevel_v1.join_group(other)`, `leave_group()`, `activate_group(token)`,
  `shelve_group()`, `unshelve_group()`, `set_group_name(string)` since 3.
- `lege_shell_manager_v1.set_terminal(app_id)` request, `terminal_requested` and
  `group_cycle_binding` events since 5 (§4.6).
- Both ends still speak version 4; raising `SHELL_PROTOCOL_VERSION` on each side is part of
  the implementation, and `ShellToplevel` gains `group: Option<GroupInfo>`. About half a day.

### WG4 — the Home tile

- `TileKind::WindowGroup { identifier: String }`, a container (alongside `Project`), top level
  only in v1. Its children are the `ParkedSession` tiles of its members.
- `merge_sessions` places a grouped window's session tile inside the group tile, creating the
  tile at the first member's place; a window that leaves the group has its tile moved out to
  the nearest free cell; an empty group tile is swept exactly like an orphaned session tile.
- Gestures: drop a session tile on another session tile → `join_group`; drag a tile out of a
  group tile onto the bare field → `leave_group`; drag the group tile → the container moves
  as one (existing container move, no reflow of its interior); click the group tile's title →
  `activate_group`; Park on the tile → `shelve_group`; rename → `set_group_name`.
- Representation ladder: Micro = count badge; Small = name and "3 windows"; Wide = name plus
  member titles in a row; Large = member previews; Full = zoomed, the existing project zoom.
- Visual baseline: a group tile joins the fixture. About 2 days.

### WG5 — hands-on acceptance

Group two windows by dropping one band on the other and see the dropped window land beside
the target with one outline around both; park and return them from Home; move the tile;
`Alt+Tab` stays inside, `Super+Tab` crosses; a band drag moves both, `Shift`+drag one; close
one of two and the group dissolves; `Ctrl+Tab` still reaches the browser's tabs. Manual check
on the work record.

## 4. The application integration choices

### 4.1 Hotfix path (in progress, agent A)

hostd root cause and fix; `lege dev deploy` continues activation past a failed unit and
reports every outcome; the shell is restarted even when a system unit fails. Until it lands:
after a failed deploy the packages are installed, and `systemctl --user restart
lege-shell.service` activates the shell by hand.

### 4.2 Search: the Home field is a launcher, and fsearch is the search

Decision: the field at the top of Home is the application launcher, labelled **Apps**, and
answers with applications only — the shape of COSMIC's application launcher, nothing more.
A file search belongs to the Files object, scoped to what it is showing. Both landed this
session. This supersedes the "federated omnibox" half of WS2 in the consolidation roadmap;
the adaptive Files surface half stands.

Real search gets a surface of its own, not a dropdown. The Files object's search field, once
it has a query, expands into a **search panel**: a partial-screen sheet (the Large rung; about
two thirds of the field's width and height, centred) listing directories then files from
lege-filed's scoped index with the path, size and modification time per row, keyboard
navigable, `Escape` back to the object. Its last row is always "Search everywhere with
FSearch", and `Super+F` from anywhere opens the panel over the person's home root. FSearch
itself is the Full rung: an application window, launched through the provider role below.

fsearch integration (interim):

- `ProviderRole::FileSearch`, exclusive, default candidate `io.github.cboxdoerfer.FSearch`
  (installed here as `fsearch`), selectable in Settings › Providers like every role.
- The Files object's search field gets a last row "Search everywhere with FSearch", and the
  context menu a "Search here…" verb, both launching the provider with `--search <query>`
  through the launch chain (identity from the cgroup as for any application).
- fsearch keeps its own database; lege-filed keeps indexing for the scoped, in-place search.
  Two indexes are accepted for now. A later step points fsearch's include list at the same
  roots `Files.Search1.Roots` reports, from sessiond, so the two agree on what is searched.
- Not planned: reading fsearch's database format, or replacing `Files.Search1`.

### 4.3 Kitty as the default terminal

- `ProviderRole::Terminal`, exclusive, default `kitty`; `Ctrl+Alt+T` (a new reserved chord,
  colliding with nothing an application binds) launches the selected terminal; "Open terminal
  here" in the Files object context menu launches it with `--directory <path>`.
- Desktop entries with `Terminal=true` launch inside the selected terminal (appd launcher
  wraps `Exec` with the terminal's `-e`/`--` form declared per provider).
- Ship `/etc/xdg/kitty/kitty.conf` from `legeos-shell` (kitty reads `XDG_CONFIG_DIRS`): the
  LegeOS palette, `background_opacity` as the user has it, `remember_window_size no` so the
  compositor's placement wins, `wayland_titlebar_color system`. The user's own
  `~/.config/kitty` overrides all of it.
- Integration beyond that (a kitty remote-control socket for "reuse the terminal in this
  project") is a follow-up under projects (WS4).

### 4.4 The Files object earns its keep

The user is right that a first-party file manager only pays off if it is better at the
common jobs. The interim list, in order:

1. Secondary-click menu on rows and on empty space, reusing `surface/context_menu.rs` and
   the declared `files.*` verbs so the menu, the toolbar and the cheat sheet can never
   disagree. Rows: Open, Open with…, Open terminal here, Copy path, Rename, Move to trash,
   Properties. Empty space: New folder, Paste, Select all, Sort by…, Show hidden.
2. Multi-select (click, Ctrl+click, Shift+click, Ctrl+A) driving the bulk verbs.
3. Keyboard verbs inside the surface: F2, Delete, Ctrl+C/X/V, Ctrl+Z, Backspace up, Enter open.
4. Drag-and-drop between the Files object, folder tiles and the shortcuts pane, using the
   `wl_data_device` transport already in progress.
5. Properties as a representation, not a dialog: the selected file's detail in the pane.

### 4.5 Screen timeout (in progress, agent B)

Compositor idle timer, outputs off on expiry and on again on input, the timeout from Power1's
stored idle policy, and a "Screen turns off after" control in Settings. Dimming and lock attach
to the same timer later (session-lock-client is the record for lock).

### 4.6 The drop-down terminal (`Super+grave`)

The user asked for `~` to open the terminal, and for a second press to put it away without
closing it. A bare `~` or `` ` `` cannot be a global chord: both are typed characters (the home
directory, Markdown, every shell), and a compositor grab would take them from every
application. So the chord is `Super+grave`, on the same reasoning that put group cycling on
`Super+Tab`, and the behaviour is exactly the one asked for:

- no terminal window: the compositor sends `terminal_requested`; the shell launches the
  `terminal` provider through the ordinary launch chain (identity from the cgroup);
- a terminal window exists but is shelved, or is not the focused window: unshelve it if
  needed and activate it;
- the terminal is the focused window: shelve it (park, never close, so an accidental press
  costs nothing) and return focus to the window that had it before.

"The terminal" is the most recently activated window whose canonical app id is the selected
`terminal` provider's, which the shell tells the compositor with `set_terminal` on connect and
on every change. With the shell dead the chord still toggles an existing terminal; only the
launch needs the shell. `Ctrl+Alt+T` (§4.3) launches a *new* terminal window every time and
is unchanged. Kitty is the default provider; the chord is provider-agnostic.

## 5. Order of work

1. Merge agent A (hostd, deploy continuation, `wayland-N`) and agent B (compositing order,
   focus, screen timeout); build set; user deploys; logout for the compositor. *Done at
   `8880b9b`; awaiting the hands-on run.*
2. Protocol declared (`9d8e3f6`). Then, in two lanes:
   - **Groups lane:** WG1 → WG3 (compositor and shell bridge) → WG2 → WG4, with WG5 as the
     user's hands-on.
   - **Applications lane:** §4.6 terminal chord with §4.3's `Terminal` role, then §4.2's search
     panel with the `FileSearch` role, then §4.4 items 1–3.
3. §4.4 items 4–5 and §6 after the next hands-on run.

## 6. Appearance: two themes, and TempleOS as a design language for Home

Not implemented in this pass; a decision recorded so the two lanes above do not paint
themselves into a corner. Renaming (the shell's labels and the product's name) is a separate,
later step and is not part of this.

The user is weighing two themes: a **monotone** theme, and a **Temple** theme that borrows
TempleOS's design language. What is worth borrowing from TempleOS is not its palette but its
premise: *the home screen is itself a working information environment, not a backdrop holding
shortcuts to one.* That is what the Home Field and the representation ladder already are, so
the homage is a sharpening, not a change of direction:

- **A working field, not wallpaper.** A light (or dark) ground that exists to carry
  information. `shell.wallpaper.enabled` already defaults off; it stays off in both themes.
- **Thin, explicit geometry.** One-pixel rules and hairline borders instead of shadows,
  glass and cards; padding measured in text lines, not in 48-pixel gutters. The density rule
  is TempleOS's; the antialiasing, scaling and spacing are modern.
- **The status line.** The information row becomes a single compact top line: clock, then
  only conditions that are true (memory pressure, a running operation, battery below a
  threshold, no network), in a semi-monospace face.
- **Typography as structure.** Headings, rules and indentation group things; borders are for
  containers only (folders, trays, projects, window groups).
- **Objects are useful where they are.** The ladder already does this (a file at Micro is a
  name and an age; at Large its first paragraphs; at Full the document). The Temple theme
  makes the rungs feel like one document growing, not like a card opening a window.
- **Palette.** Base: off-white ground, near-black text, neutral hairlines. Temple accent:
  a saturated cyan and a royal blue for structural lines, focus and links (the VGA pair).
  Red, green, yellow and magenta only for semantic state. The monotone theme keeps the same
  structure with a single accent hue and grey rules. Neither theme reproduces the 16-colour
  palette across the whole shell.
- **Faces.** A monospaced or semi-monospace display face for Home headings and system
  readings; the proportional reading face stays for document text.

Mechanically this is a `Theme` value in `surface/design.rs` (today's constants become one of
two token sets selected by a `shell.appearance.theme` setting), with the group accent palette
(§2 rule 7) defined per theme. The visual baseline fixture renders once per theme.
