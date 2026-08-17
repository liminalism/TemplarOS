# ADR-0011: Flatpak for sandboxed apps, no snapd; telemetry-free crash handling

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Decision
Sandboxed applications come as Flatpak (the alias model has Flatpak IDs); `snapd` is excluded from the seeds
(Ubuntu's Firefox/Thunderbird snaps are replaced by Flatpak/deb policy). This is not "a new app store".
Crash handling: `systemd-coredump` on, `apport`/`whoopsie` out; sessiond ingests coredump journal entries
(PID → cgroup → instance) into the event history so "X crashed at 14:32 — restore?" is one searchable event.
