# ADR-0014: systemd-oomd is the backstop, resourced owns the session

* **Status:** accepted
* **Date:** 2026-09-01
* **Relates to:** ADR-0002 (resourced is a separate, protected process)

## Decision

Two things on a LegeOS desktop can decide that a process must lose memory: `lege-resourced` and
`systemd-oomd`. Until now nothing anywhere said which of them was in charge — the string `oomd`
appears nowhere in the tree — so the answer was whatever the distribution's defaults happened to
be. This settles it.

* **Inside the session, `lege-resourced` is the only pressure actor.** It is the one that knows
  which window has focus, which application has unsaved work, which instance belongs to the
  project the user is in the middle of, and which are disposable. `systemd-oomd` knows none of
  that: it picks by cgroup pressure and swap usage.
* **`systemd-oomd` stays armed on `app.slice` as a far-above backstop**:
  `ManagedOOMMemoryPressure=kill` with `ManagedOOMMemoryPressureLimit=90%`. Ninety percent
  sustained memory pressure on the whole application slice is not a situation resourced is
  handling badly — it is a situation in which resourced is very likely not running, or is itself
  stuck. Something has to act, and the kernel's OOM killer acting first is worse.
* **`systemd-oomd` is disarmed on `session.slice`.** The compositor, the shell, sessiond, filed
  and the governor are what a recovery is *for*; a backstop that can kill them is not a backstop.
* **The five session units declare `ManagedOOMPreference=avoid`**, so that even if oomd is armed
  somewhere they are visible to — a distribution default, a future slice reorganisation, a user's
  own drop-in — it looks elsewhere first.
* **While resourced is at `ObserveOnly`, oomd is deliberately the only actor.** That is the
  current state: resourced records what it *would* do and does none of it, so removing the
  backstop as well would leave a desktop with no containment at all. This is the reason the
  arming is not conditional on resourced's stage.

## Context

`lege-resourced` ships observe-only (ADR-0002, planning doc 02 §8: only after recorded
playthroughs show sensible choices should it be allowed to act). That makes the question of who
else may act urgent rather than theoretical, and it had no answer. Ubuntu 26.04 ships
`systemd-oomd` enabled; without a stated posture, a LegeOS session would inherit whatever
`user@.service`'s defaults do, which on some configurations includes killing under swap pressure
in a session slice this project spends considerable effort protecting.

The two actors are not interchangeable and the difference is not a matter of tuning.
`lege-resourced` refuses to consider protected instances at all, weighs foreground and visibility
and unsaved work, prefers trimming to throttling and throttling to termination, and records why.
`systemd-oomd` applies one rule to a whole slice. As a *first* responder it would undo the work;
as a *last* one it is exactly right, because by the time it fires the first responder has already
failed.

## Consequences

* `distro/policy/systemd/user/app.slice.d/legeos.conf` arms oomd at 90%;
  `session.slice.d/legeos.conf` explicitly does not, and says so rather than relying on a default.
* `lege-comp`, `lege-shell`, `lege-sessiond`, `lege-filed` and `lege-resourced` carry
  `ManagedOOMPreference=avoid`.
* `lege doctor`'s `oomd-ownership` check reads those files back on the installed system and warns
  when the posture has drifted — a distribution upgrade replacing a drop-in is the realistic way
  that happens.
* Disarming oomd on `session.slice` removes a backstop from the units that must survive. What
  remains for them: `MemoryLow` on each unit and on the slice, `OOMScoreAdjust` between -500 and
  -1000, resourced's own structural exclusion of everything under `session.slice` from the victim
  pool, and, last, the kernel OOM killer — which honours `OOMScoreAdjust` and will therefore reach
  for an application long before it reaches for the compositor. That is a real reduction in
  defence, accepted knowingly: the alternative is a backstop whose only available action against a
  thrashing session is to kill the recovery surface.
* When resourced is eventually raised past `ObserveOnly`, this ADR does not change. The 90% limit
  is far enough above resourced's own `Critical` that the two do not race; if traces later show
  them overlapping, the limit moves, not the ownership.
