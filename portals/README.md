# portals/ — `xdg-desktop-portal-lege`

The single LegeOS portal backend (`org.freedesktop.impl.portal.desktop.legeos`). It serves the four
interfaces `legeos.portal` claims, and only those: FileChooser → `lege-filed` (shared last-folder
policy, filters, hidden files), AppChooser → `Associations1` + `AppIdentity1`, Background →
providers/autostart, Settings → `Settings1`.

**There is no OpenURI backend, and there is nothing to write one against.**
`org.freedesktop.impl.portal.OpenURI` does not exist: `xdg-desktop-portal` (1.20.3 on the Ubuntu
base) implements `org.freedesktop.portal.OpenURI` itself. It finds the handler through
GAppInfo/`mimeapps.list` — the file `lege-associationd` generates — asks *our* AppChooser only when
it has to ask, and launches the chosen desktop entry in its own scope. So "open this downloaded
file" is served here by AppChooser, and LegeOS's own defaults reach the frontend through that
exported `mimeapps.list`, not through a portal interface. Two consequences worth knowing:
the launch does not go through `Launcher1`, so a file opened this way is not started into a LegeOS
cgroup; and the export is regenerated on every association change but not at session start, so a
home whose `mimeapps.list` was never written can disagree with `Associations1` until something
changes a default.

`legeos.portal` + `legeos-portals.conf` select this backend for `XDG_CURRENT_DESKTOP=LegeOS`;
ScreenCast/Screenshot stay pinned to the cosmic portal fork until covered, and Print, Email,
Notification and DynamicLauncher fall to `xdg-desktop-portal-gtk`. Backend selection is the
`portal-backend` provider role; sessiond regenerates the user's `portals.conf`.
`docs/planning/daily-driver-matrix.md` records what each of these actually delivers today.
