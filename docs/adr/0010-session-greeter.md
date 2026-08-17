# ADR-0010: systemd-managed session; greetd + cosmic-greeter; compositor-owned lock

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Decision
The session is `legeos-session.target` + user units (systemd DESKTOP_ENVIRONMENTS convention) started by
`lege-session` from `wayland-sessions/legeos.desktop`; `xdg-desktop-autostart.target` is masked — sessiond
launches approved autostart entries with identity + cgroups. Greeter: `greetd` + the `cosmic-greeter` fork
(reuses the compositor; no second toolkit stack). Lock screen: compositor-core `ext-session-lock-v1` — one
provider by construction.
