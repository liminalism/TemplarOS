# portals/ — `xdg-desktop-portal-lege`

The single LegeOS portal backend (`org.freedesktop.impl.portal.desktop.legeos`): FileChooser → `lege-filed`,
AppChooser/OpenURI → `Associations1`, Background → providers/autostart, Settings → `Settings1`.
`legeos.portal` + `legeos-portals.conf` select it for `XDG_CURRENT_DESKTOP=LegeOS`; ScreenCast/Screenshot
stay on the cosmic portal fork until covered. Backend selection is the `portal-backend` provider role;
sessiond regenerates the user's `portals.conf`.
