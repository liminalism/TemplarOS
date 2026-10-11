# shell/ — the restartable shell (`templar-shell`)

Homegarden presents current work alongside activity-grown Places, pinned objects and recent
files/folders. `field_model.rs` merges the compositor's event-driven window snapshot;
`surface/field_screen.rs` draws open or minimized windows in their project's Now strip or the
bounded Loose tray, using existing preview and window-action paths. `work_layout.rs` allocates
those slots from verified instance/project context. Parking, restoring or activating a window
does not rearrange the persistent project field.
Terminals, file managers and image viewers are resolved from catalogue categories and provider
roles. Windows that do not
fit remain reachable through Loose/overflow, including a "+N windows" card that opens the
window overview. Clicking (press and release without moving) activates the same window or
restores it from minimization and leaves Home. Crossing the lift threshold starts a drag;
linking a window to a project requires a broker-validated association, and Escape cancels.
Closing removes its transient card without clearing recents or Places. Alt-Tab keeps the
compositor's frozen order and key/release behavior;
the shell now paints its live-preview/icon switcher on a Presentation surface, with the compositor's
text HUD retained when the shell is absent. Cards never edit the field document, so window lifecycle
does not reflow persistent objects. Each frame tells the compositor every window's card rectangle
(`set_minimize_target`), which anchors park/restore transitions. See
[Homegarden](../docs/architecture/homegarden.md) for garden ownership and policy.

Persistent tiles retain drag, resize, selection, file operations and undo through
`crates/templar-field` and `org.templaros.HomeField1`. Files opens folders in the selected
file-manager provider. Window previews use the compositor's standard capture protocols
(`previews.rs`; `shell.home-field.previews` = off / static / live). The shell renders and
forwards actions; service-owned recents, projects, settings and garden state stay in their
existing services.

`launcher_model.rs` and `surface/launcher_screen.rs` provide one focused Apps/Files popup;
`surface/launcher_host.rs` owns its overlay, search workers, activation and metadata commits.
Super+Space, the bottom rail and Home's **Open launcher** action share that popup. It does not
reveal Home or rearrange the current application. Apps starts with favorites and recents;
Tab switches to Files while retaining text, and Files searches Index1 directly. Click or
Enter opens immediately; Ctrl+Enter requests a new application/profile instance; Escape or
focus loss dismisses. Search generations fence stale results, and pointer actions freeze
their native target. Ordinary activation never creates a Home tile. Explicit context actions
save rail favorites or Home shortcuts, edit display aliases, show information and hide results.

The application rail reconciles persisted favorites with `taskbar.rs`'s authoritative live
task units. A saved plain app and its live task share one chip; terminal profiles preserve
their distinct directory/arguments, including copied Home shortcuts. `Launcher1` remains
the launch authority; files use Associations1 and brokered opening. FSearch routing and its
special row/footer are retired. See [the launcher guide](../docs/launcher.md).

Home, the bottom bar, gesture rails and switcher use the same icon/activity-stripe/park-dim marks.
Home names appear on hover or keyboard focus. Preview wells letterbox their original aspect.
Group bar chips stack
up to three member icons. Settings, network and account controls use geometric symbols and hover
tooltips; the clock retains text.
Project slots and Loose preserve access at narrow sizes through explicit overflow. Verification
asserts containment, actions and reachability rather than fixed coordinates or paint hashes.

`shell.home-field.reduced-motion` also governs compositor park/restore transitions and rail reveal.
Setting it during a transition cancels that visual motion without changing the already-committed
window state. Appearance & Motion selects Essence, Simple or Off and particle size (default
6 logical px, range 1–24). Essence runs in the compositor's GLES scene; shader or snapshot
capability failures use the existing simple motion. Grain size changes only drawing uniforms.
The material and bar shader experiments stay in the effects sandbox.
Soft normalized folder centroids live separately at
`$XDG_STATE_HOME/templar-shell/work-positions.json` (or `~/.local/state/templar-shell/`), survive
session/grid/project changes and yield to occupied cells. Client-provided app-id claims are bounded
display hints with a `?` marker, never trusted identity, capabilities or application grouping.

Structured Recent entries retain path, application, timestamp and MIME through both Files views;
clicks use their actual path rather than re-parsing decorative row text. Shell logs disable ANSI
when stdout is not a TTY; missing icon lookups are informational once per application.

The bottom-right time button opens a native clock/calendar in the machine cluster, independent
of installed Calendar-category applications. [Chrono](https://docs.rs/chrono/0.4.45/chrono/)
provides Gregorian date arithmetic; the existing OS-local clock supplies today's date/time.
Previous/next month, Today, date selection, arrows, Page Up / Page Down, Home/T, Tab and Escape
are supported. Calendar and account popovers are mutually exclusive. Focus loss dismisses the
calendar; leaving it with the pointer alone does not. It remains a local date browser, not an
appointment/reminder store. No second GUI toolkit or timezone database is added.

Headless UI verification (no display or service dependencies for these fixtures):

```sh
templar-shell --render-home-work /tmp/home-work
templar-shell --render-calendar /tmp/calendar
templar-shell --render-switcher /tmp/switcher
templar-shell --render-rails /tmp/rails
templar-shell --render-launcher /tmp/launcher
```

These commands exercise the real drawing/input code and emit normal, narrow, fractional-scale,
HiDPI and rail frames as appropriate. They assert same-window actions, contained project slots,
Loose card placement that survives parking and activation, the overflow card,
closure and recents preservation, plus switcher overflow/selection, capture refresh, icon/park marks,
calendar navigation, Today, dismissal and input geometry. The launcher command emits Apps/Files
frames at desktop, narrow and HiDPI sizes without a session bus; launcher interaction tests
cover native paths, aliases, input and scrolling. Switcher captures are deterministic
fixtures, not photographs of a session. These commands do not establish live hardware/composed-session
behavior; native park/restore feel and the reported intermittent bottom band require hardware observation.
