# ADR-0002: The resource governor is a separate, tiny, protected process

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Context
Under memory pressure the governor must act while everything else stalls: a daemon doing SQLite/fsync/D-Bus/
desktop-file imports (sessiond) can block on a full disk — the very storage-reserve case — or panic in an
unrelated domain.

## Decision
`lege-resourced` is its own process: `mlockall`, `MemoryLow`, `OOMScoreAdjust=-1000`, high CPU/IO weight,
`Nice=-10`, no SQLite. It needs **no D-Bus round-trip for emergency actions**: launch units are self-describing
(`app-legeos-<AppId>@<InstanceId>.service`, `lege_types::unit`), so cgroup→instance comes from
`/sys/fs/cgroup` alone; foreground/visibility arrive as compositor signals; sessiond only enriches. Policy
is a pure engine (`policy::Governor`) separated from cgroup writes; **observe-only by default** until traces
are reviewed, then cache-trim → throttle → checkpoint → kill are enabled in stages.

## Consequences
Two user daemons for policy instead of one; the governor is testable in isolation and survives sessiond
restarts. `lege doctor` verifies its protection values.
