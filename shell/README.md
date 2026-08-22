# shell/ — the restartable shell (`lege-shell`)

The Home Field (`surface/field_screen.rs` over `field_model.rs` and `crates/lege-field`): the desktop
surface as a bounded grid of tiles — every application, every window (open or parked), and the drawers
(`home_surface.rs`) as tiles whose full bodies open in place. Windows collapse into tiles ("park") and
expand back out; dragging a tile by its title band moves it with a ghost of where it will land and the
operation it will perform spelled out first; dropping one loose tile on another makes a project (a real
`Projects1` project, rooted under `~/Projects/<id>`), and every `Projects1` project has a tile whose body
zooms the field into its interior grid — a view with a breadcrumb back, never a hidden desktop. The dock
(`surface/dock_screen.rs`) groups by canonical AppId and carries the Home cell that reveals the field.
Every action has a keyboard route (arrows, Enter, Space, Escape, Delete, Ctrl+Z) and everything the field
shows is `lege-shell --print field`. Never owns state: everything comes from `org.legeos.Session` /
`org.legeos.Files` and the compositor's `lege_shell_v1`; the field's own layout is kept for the session
until `org.legeos.HomeField1` persists it.
