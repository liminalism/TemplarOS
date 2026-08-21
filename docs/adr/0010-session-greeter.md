# ADR-0010: systemd-managed session; greetd + cosmic-greeter; compositor-owned lock

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Decision
The session is `legeos-session.target` + user units (systemd DESKTOP_ENVIRONMENTS convention) started by
`lege-session` from `wayland-sessions/legeos.desktop`; `xdg-desktop-autostart.target` is masked — sessiond
launches approved autostart entries with identity + cgroups. Greeter: `greetd` + `lege-greeter`. Lock screen: compositor-core `ext-session-lock-v1` — one
provider by construction.

## Amendment (2026-08-21, ADR-0012)
The greeter is `lege-greeter`, not the `cosmic-greeter` fork. The original reasoning — reuse the
compositor, avoid a second toolkit stack — still holds and is better served now: `lege-greeter`
uses pixelkit, the same toolkit as the shell, so there is no second stack at all, whereas
`cosmic-greeter` would have brought libcosmic and iced back in for the login screen alone.

It authenticates by speaking greetd's JSON IPC over its socket, so greetd owns PAM and the
greeter links no authentication code of its own. The lock screen is unchanged: compositor-core
`ext-session-lock-v1`, which Smithay implements — one provider by construction.
