# Replay fixtures — hand-authored, not recordings

These four traces are **written by hand**. Nobody's desktop produced them. They are scenarios
chosen to exercise the parts of the governor that are hard to reach otherwise, in the format
`lege-resourced` records (`crate::trace::TraceRecord`, one JSON object per line), so
`lege-resourced --replay` can push them back through the current policy.

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
cargo run -p lege-resourced -- --replay tests/fixtures/resources/replay          # diff
cargo run -p lege-resourced -- --replay tests/fixtures/resources/replay --bless  # re-record
```

Blessing is how a deliberate policy change gets committed, *after* its diff has been read and
quoted in the commit message. It is never the way to make a surprising diff go away.

## The four scenarios

**`01-calm-desktop.jsonl`** — 20 GiB free, no pressure, four instances including the protected
shell. Every tick should be `normal` and should emit nothing but `restore-limits`. The trace that
catches a policy change which starts acting when nothing is wrong.

**`02-elevated-background-build.jsonl`** — a build growing in the background while the user types
in the editor. `elevated` throughout; the disposable browser is chosen and trimmed, and the
foreground editor is left alone. The trace for "does mild pressure stay proportionate".

**`03-severe-foreground-runaway.jsonl`** — memory falling to `severe` while the *foreground*
browser is the thing growing at 420 MiB/s. Today the foreground protection outweighs its size and
the background build is chosen instead. That is worth knowing rather than assuming, and it is
exactly the behaviour the roadmap's reclaim-plan work changes; when it does, this trace is where
the change shows up.

**`04-critical-then-recovery.jsonl`** — `critical` held past the three-second emergency delay so
the kill decision is reached, then memory freed. Two things are asserted at once: that the kill is
decided from the *record's own* clock rather than how long a replay took, and that the level takes
three good ticks to come back down, because de-escalation is debounced and escalation is not.

Note that `stage` is `observe-only` in every record: these are decisions, not actions taken. At
that stage the executor logs what it would have done and does none of it.
