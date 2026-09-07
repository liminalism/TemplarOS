# The throw experiment

`@legeos.decision.home-field-stable-bounded-semantic-grammar` stages *throwing* a tile — a release with velocity projects it toward a
destination — as accelerated direct manipulation, and the author's brief was plain: it may not beat
drag-and-drop, but it is worth testing. So it is built as an experiment with a kill switch, not a
feature with a roadmap.

## What exists

* `crates/lege-field/src/drag.rs`: velocity is an exponentially weighted average over an 80 ms
  window; projection is closed-form (`d = v²/2f` along the velocity), clamped to the field, so
  the ghost drawn before release is exactly where release lands. `DragConfig::throw_enabled`
  gates it; below `throw_min_speed_px_s` (600) a release is a precise placement regardless.
* `shell.home-field.throw` (`Settings1`): `off` (the default; drag-and-snap only) or `full`.
* While `full`: every pointer drop appends one JSON line to
  `$XDG_STATE_HOME/lege-shell/throw-spike.jsonl` —
  `{"t", "event":"drop", "thrown", "lifted_for_ms", "semantic", "relift", "op"}` — and an undo
  within five seconds of a drop appends `{"t", "event":"undo-after-drop", "since_drop_ms"}`.
  Nothing is logged while `off`, and nothing leaves the machine.

## What is measured

| Measure | From |
|---|---|
| time-to-target | `lifted_for_ms`, lift to release |
| error rate | `undo-after-drop` within 5 s, plus `relift` (the same tile dropped again within 3 s) |
| semantic share | `semantic`: the drop named an operation rather than placing |

Compare thrown drops (`"thrown":true`) against the rest in the same log. Both modes are the
author's own use; the bar is two weeks and at least 200 drops per kind.

## The decision rule

Keep `full` as a non-default option only if the median `lifted_for_ms` of thrown drops is at
least 15 % lower **and** their error rate (undo-after-drop + relift, over drops) is within two
points of drag-and-drop's. Otherwise delete projection: `projected_position`'s `if
cfg.throw_enabled` branch and `would_throw` (~60 lines in `drag.rs`), the setting's `full`
choice, and this logging — `Drag::Projected` then means snap-to-nearest-free-cell, which is what
the rest of the field already assumes. Either outcome is recorded in the AKR ledger against
`@legeos.work.home-field`.

```sh
jq -r 'select(.event=="drop") | "\(.thrown) \(.lifted_for_ms)"' ~/.local/state/lege-shell/throw-spike.jsonl \
  | sort | awk '{a[$1]=a[$1]" "$2} END {for (k in a) print k, a[k]}'
```
