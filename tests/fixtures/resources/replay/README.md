# Replay fixtures — hand-authored, not recordings

These nine traces are **written by hand**. Nobody's desktop produced them. They are scenarios
chosen to exercise the parts of the governor that are hard to reach otherwise, in the format
`templar-resourced` records (`crate::trace::TraceRecord`, one JSON object per line), so
`templar-resourced --replay` can push them back through the current policy.

That distinction is the whole point of this file. A green replay proves the policy makes the
**same** decisions on the same inputs — that a refactor was a refactor, and that a deliberate
change moved exactly what it meant to move. It proves nothing about whether those decisions are
*right*. The weights and thresholds in `policy.rs` say in their own comments that they are
starting values to be calibrated from real traces; these fixtures are not that calibration and
must never be cited as if they were.

Each record's **inputs** — available memory, PSI, and the per-instance memory, growth, focus and
flags — are the hand-authored part. Each record's **outcome** — `level`, `victim`, `actions` — is
whatever the policy decided when the fixture was last blessed:

```sh
cargo run -p templar-resourced -- --replay tests/fixtures/resources/replay          # diff
cargo run -p templar-resourced -- --replay tests/fixtures/resources/replay --bless  # re-record
```

Blessing is how a deliberate policy change gets committed, *after* its diff has been read and
quoted in the commit message. It is never the way to make a surprising diff go away.

## The nine scenarios

**`01-calm-desktop.jsonl`** — 20 GiB free, no pressure, four instances including the protected
shell. Every tick should be `normal` and should emit nothing but `restore-limits`. The trace that
catches a policy change which starts acting when nothing is wrong.

**`02-elevated-background-build.jsonl`** — a build growing in the background while the user types
in the editor. `elevated` throughout; the disposable browser is chosen and trimmed, and the
foreground editor is left alone. The trace for "does mild pressure stay proportionate".

**`03-severe-foreground-runaway.jsonl`** — memory falling to `severe` while the *foreground*
browser grows at 420 MiB/s. Two things this now records. The foreground browser is structurally
excluded from Severe unless it is demonstrably the cause — and at 420 MiB/s it is, being above the
runaway threshold — so the score, not the exclusion, is what decides; the background build still
wins it on size. And the plan throttles a *second* instance, because one instance's ten percent
does not cover the deficit.

**`04-critical-then-recovery.jsonl`** — `critical` held past the three-second emergency delay so
the kill decision is reached, then memory freed. Two things are asserted at once: that the kill is
decided from the *record's own* clock rather than how long a replay took, and that the level takes
three good ticks to come back down, because de-escalation is debounced and escalation is not.

**`05-low-memory-no-stall.jsonl`** — 2 GiB free and no stall whatsoever. This is the one fixture
that isolates the *memory* leg of the classifier from the PSI leg: the other four all reach their
level through PSI, so they would decide the same thing whatever the reserve was. On the 48 GiB
reference machine 2 GiB is below the 3 GiB reserve and reads as `severe`; replayed through a
4 GiB laptop's scaled thresholds it reads as `normal`, because 2 GiB free on a 4 GiB machine is
half the machine. `services/templar-resourced/tests/replay_fixtures.rs` asserts exactly that
divergence, which is how the memory-scaling work is evidenced rather than asserted.

**`06-attention-and-intent.jsonl`** — Severe pressure with three real candidates: a document the
user is typing in, the biggest process on the machine which the user has pinned, and a terminal
untouched for three quarters of an hour that can be checkpointed and restarted. The terminal is
chosen. Replayed with the attention and intent weights set to zero — the policy as it stood before
them — the *browser* is chosen instead, on size alone, because the user having asked to keep it and
the terminal having been abandoned were facts the score could not see.
`replay_fixtures.rs::attention_and_intent_change_which_instance_is_chosen` asserts that divergence,
and asserts that the other fixtures are untouched by the same change, since none of them
carries an attention signal.

Note that `stage` is `observe-only` in every record: these are decisions, not actions taken. At
that stage the executor logs what it would have done and does none of it.

**`07-pressure-trigger-wakeup.jsonl`** and **`08-no-trigger-fallback-sampling.jsonl`** — the same
episode twice: calm on the idle heartbeat, an escalation the kernel's PSI trigger woke the loop
for, then calm again. `07` is a kernel that offers a trigger on the delegated cgroup's
`memory.pressure`; `08` is one that does not, so every wake there is an ordinary interval sample.
They exist to hold one property: `wakeup` records why the loop was awake and must never become an
input to policy — the two must decide identically, and
`why_the_loop_woke_is_recorded_and_changes_nothing_it_decides` fails if they stop.

**`09-critical-victim-change.jsonl`** — one unbroken `critical` episode in which the chosen victim
is terminated and disappears while the pressure stays. The tick that first selects a *second*
victim must contain it and ask it to checkpoint, and must not kill it: the emergency delay is that
application's chance to save its work, so it belongs to whoever was chosen rather than to how long
the machine has been in trouble. It is killed three seconds later, on its own clock. Every other
fixture keeps one victim from beginning to end, which is why none of them could catch this;
`the_grace_period_follows_the_victim_rather_than_the_episode` is the assertion.
