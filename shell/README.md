# shell/ — the restartable shell (`templar-shell`)

Homegarden presents current work alongside activity-grown Places, pinned objects and recent
files/folders. `field_model.rs` merges the compositor's event-driven window snapshot;
`surface/field_screen.rs` draws every open or minimized window as a card in the field's free
cells, using the existing preview and window-action paths. `work_layout.rs` places them: by
folder (exact brokered terminal CWD, then the window's Place association, then its application's
most recent folder in the structured Recent feed), with remembered soft folder centroids.
Cards keep their positions — parking, restoring or activating a window never moves a card.
Terminals, file managers and image viewers are resolved from catalogue categories and provider roles. Windows that do not
fit are counted on one "+N windows" card that opens the window overview. Clicking (press and release
without moving) activates the same window or restores it from minimization and leaves Home; dragging
a card past the lift threshold moves it onto free cells instead, where it stays as other windows come
and go (an in-memory hint, not a field tile; a blocked drop or Escape snaps it back). Closing removes its transient card without
clearing recents or Places. Alt-Tab keeps the compositor's frozen order and key/release behavior;
the shell now paints its live-preview/icon switcher on a Presentation surface, with the compositor's
text HUD retained when the shell is absent. Cards never edit the field document, so window lifecycle
does not reflow persistent objects. Each
frame tells the compositor every window's card rectangle (`set_minimize_target`), so parking
shrinks a window into its card and restoring grows it back out. See
`docs/architecture/homegarden.md` for garden ownership and policy.

Persistent tiles retain drag, resize, selection, file operations and undo through
`crates/templar-field` and `org.templaros.HomeField1`. Files opens folders in the selected
file-manager provider. Window previews use the compositor's standard capture protocols
(`previews.rs`; `shell.home-field.previews` = off / static / live). The shell renders and
forwards actions; service-owned recents, projects, settings and garden state stay in their
existing services.

Home, the bottom bar, gesture rails and switcher use the same icon/activity-stripe/park-dim marks.
Home names appear on hover or keyboard focus; folder clusters share one faint named plate and an
unfiled cluster has no plate. Preview wells letterbox their original aspect. Group bar chips stack
up to three member icons. Settings, network and account controls use geometric symbols and hover
tooltips; the clock retains text.
Aligned adjacent cards are preferred within a folder cluster. A smaller card is used before
disconnecting that cluster; persistent objects remain obstacles and inter-cluster gaps win.
Verification asserts those behaviors, not fixed coordinates or paint hashes.

`shell.home-field.reduced-motion` also governs compositor park/restore transitions and rail reveal.
Setting it during a transition cancels that visual motion without changing the already-committed
window state. Park/restore uses a smooth geometric size curve; client content is never stretched.
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
```

These commands exercise the real drawing/input code and emit normal, narrow, fractional-scale,
HiDPI and rail frames as appropriate. They assert same-window actions, centred and
folder-clustered card placement that survives parking and activation, the overflow card,
closure and recents preservation, plus switcher overflow/selection, capture refresh, icon/park marks,
calendar navigation, Today, dismissal and input geometry. Switcher captures are deterministic
fixtures, not photographs of a session. These commands do not establish live hardware/composed-session
behavior; native park/restore feel and the reported intermittent bottom band require hardware observation.
