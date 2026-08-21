# Resources (`lege-resourced`)

The governor is a separate, tiny, protected process (ADR-0002). It must keep deciding while
everything else stalls, so it holds no SQLite connection, does no fsync on its loop, and needs
**no D-Bus round-trip to contain a runaway application**: launch units are self-describing
(`app-legeos-<AppId>@<InstanceId>.service`), so cgroup → instance comes from `/sys/fs/cgroup`
alone.

## Pressure states

Normal → Elevated (trim background caches) → Severe (`memory.high` plus proactive reclaim on the
highest-scoring victim) → Critical (checkpoint request; kill the cgroup after
`emergency_kill_delay`). Levels carry hysteresis so the state does not flap around a threshold.

The victim score weighs memory, growth rate, PSI contribution, foreground and visible state,
unsaved work and disposability — **never size alone** — and returns a per-term breakdown, so
`ExplainInstance` and `lege res` can say why, in numbers, and the history event can read like a
sentence rather than a code.

## Two independent protections

Session-critical units (compositor, shell and its recovery overlay, audio, the governor itself)
are protected twice over: **structurally**, because they live under `session.slice` and the
victim scan only ever looks at `app.slice`, so they cannot enter the pool however the policy
behaves; and **by the kernel**, through a modest `memory.low` written to each one. `memory.low`,
never `memory.min` — overcommitted hard protection can itself force an OOM.

Application cgroups get `memory.oom.group=1` once, at registration, so a workload dies as one
unit rather than leaving helper wreckage. Neither of these is gated by the containment stage:
they are structurally protective, unlike the actions below, which can go wrong.

## The containment ladder

`observe-only` → `trim` → `throttle` → `checkpoint` → `terminate`, each enabling everything below
it, defaulting to **observe-only**. An action above the current stage is decided, scored, logged
and signalled with `applied = false`, and not performed. Planning doc 02 §8 is explicit that
`memory.high` and proactive reclaim wait for recorded playthroughs that show sensible choices,
and that termination comes last.

The traces are that evidence: a bounded JSONL ring under `$XDG_STATE_HOME/legeos/`, readable back
through `RecentTraces`, recording pressure episodes and every decision with the score breakdown
that produced it. Idle ticks are not episodes and are not recorded — a ring full of nothing is a
ring that has rotated away the thing you needed to see.

## What the governor does not own

Application memory exhaustion is one failure among several, and the others need different
recovery paths and different messages. `NonMemoryStall` reports what can honestly be detected
without privilege: **storage** stalls (I/O pressure without memory pressure), **GPU resets**
(sysfs reset counters), and **kernel lockups** (hung-task and RCU-stall evidence, where
`kernel.dmesg_restrict` permits reading it — the detector stays silent rather than guessing when
it does not). Hardware failure is not detected: no unprivileged signal for it exists, and saying
so is better than pretending.

## Swap

Real swap on NVMe with a bounded zswap cache in front of it, declared in `distro/policy/`
(`sysctl.d`, `tmpfiles.d`, kernel command line). Compressing pages in RAM before writing them
trades CPU for swap I/O; the disk-backed layer is the escape when compressed memory fills.
