# ADR-0001: One user session policy daemon (templar-sessiond)

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
The planning docs propose `appd`, `associationd`, `resourced`, `filed`, `updated` as services and note
"associationd, probably as part of appd". The user's Lubuntu install suffers from fragmentation: many daemons
and applets, each with private state and no shared identity key. Cross-daemon D-Bus adds races the docs
themselves flag (launch registration vs first window; a governor blocking on identity lookups).

## Decision
`templar-sessiond` hosts application identity + launching, the association broker, provider/autostart
arbitration, history, notifications (`org.freedesktop.Notifications` when selected), typed settings, project
sessions, power policy and timezone/location policy — as **separate library crates** wired by a thin daemon,
over **one** SQLite state store (`templar-state`). Bus name `org.templaros.Session`; domain-scoped, versioned
interfaces (`org.templaros.AppIdentity1`, …) at separate object paths so a later split only relocates a bus name.
`Type=notify`, `Restart=on-failure`, `WatchdogSec`; instances persisted and re-derived from
`ListUnitsByPatterns app-templaros-*` on restart; history writes non-fatal.

## Consequences
One crash/restart unit and one place to explain decisions; cross-domain updates are transactions. The file
service and the resource governor stay separate (ADR-0002); the compositor treats sessiond absence as
"provisional identity". Crate boundaries keep the split reversible in a day.
