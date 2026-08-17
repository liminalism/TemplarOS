# shell/ — the restartable shell (`lege-shell`)

Home Surface drawers (`home_surface.rs`), panel/dock grouped by canonical AppId, launcher, window shelf,
notification renderer, applets-as-views. libcosmic/iced UI lands with the first surface. Never owns state:
everything comes from `org.legeos.Session` / `org.legeos.Files` and the compositor's `lege_shell_v1`.
