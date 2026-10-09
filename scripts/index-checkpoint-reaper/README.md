# Index-checkpoint reaper (TEMPORARY mitigation)

## Why this exists

`templar-filed` plants one read-only `@home` snapshot per downtime catch-up
and discards it on the way out (`services/templar-filed/src/delta_catchup.rs`).
A crash between plant and discard/retire orphans the snapshot at
`/.snapshots/index/`, and nothing reaps orphans today: two orphans just
pinned ~250 GB until the disk filled. This directory holds a small user-level
reaper that sweeps orphans through the **existing** hostd D-Bus API. It adds
no shipped service, changes no shipped file, and needs no rebuild.

## How it stays safe

`reap-index-checkpoints` (python3, stdlib only) calls `List` (an
unauthenticated read) and then `Discard` — but only for a checkpoint where
ALL of these hold:

1. the id matches hostd's `is_safe_id` grammar (hostd checks again anyway);
2. the id is NOT named as `checkpoint` by any known filed baseline file
   (`~/.local/state/templaros/delta-checkpoint.json` by default, plus
   `--baseline` / `TEMPLAR_REAPER_BASELINES`);
3. `created_ms` is known AND the checkpoint is older than `--min-age-min`
   (default 60). filed's only other references to an id are in-memory
   candidates held for seconds during a catch-up, so the grace period keeps
   the reaper from ever racing one.

It fails closed: a present-but-unparseable baseline file aborts the run, a
malformed `List` reply aborts the run, and a `Discard` failure stops the run.
It is a dry run unless `--apply` is given. Discarding a checkpoint can never
corrupt the index — filed treats a missing baseline as "reconcile instead".

`Discard` is polkit-gated by `org.templaros.host.index-checkpoint`, whose
`allow_active` is `yes`, so the timer works unattended from your session.

## Install (run these yourself — the agent does not touch your live session)

```sh
# 1. Install the script where the unit expects it.
install -m 0755 scripts/index-checkpoint-reaper/reap-index-checkpoints ~/.local/bin/templar-reap-index-checkpoints

# 2. Dry run first: must report what it would do and discard nothing.
~/.local/bin/templar-reap-index-checkpoints

# 3. Install and enable the hourly timer.
cp scripts/index-checkpoint-reaper/templar-index-checkpoint-reaper.service \
   scripts/index-checkpoint-reaper/templar-index-checkpoint-reaper.timer \
   ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now templar-index-checkpoint-reaper.timer
```

Verify with:

```sh
systemctl --user list-timers templar-index-checkpoint-reaper.timer
journalctl --user -u templar-index-checkpoint-reaper.service --since today
```

A one-shot sweep without the timer is just step 1 plus:

```sh
~/.local/bin/templar-reap-index-checkpoints --apply
```

## Tuning

- `--min-age-min 60` (default): never discard anything younger. Lower it for
  a one-shot only if the disk is actively full; the default is the safe one.
- `--max-discards 16` (default): oldest-first cap per run; leftovers wait for
  the next hour. Raise it for a one-shot backlog.
- `--baseline PATH` (repeatable) or `TEMPLAR_REAPER_BASELINES` (colon
  separated): extra filed baseline files to treat as live, for setups that
  run filed with a custom `--state-db`.

## Uninstall

```sh
systemctl --user disable --now templar-index-checkpoint-reaper.timer
rm ~/.config/systemd/user/templar-index-checkpoint-reaper.service \
   ~/.config/systemd/user/templar-index-checkpoint-reaper.timer \
   ~/.local/bin/templar-reap-index-checkpoints
systemctl --user daemon-reload
```

## Known limits (why this is temporary, not the fix)

- Single-user view: the reaper only knows the baseline files it is told
  about. On a multi-user machine, pass every user's baseline file via
  `--baseline`, or one session could discard another's live baseline (the
  cost is only a slower reconcile, but still). The shipped polkit note
  already flags per-uid scoping as unbuilt.
- It bounds accumulation to roughly one hour of crash-orphans; a tight
  filed crash loop could still outpace the hourly cap. Raise
  `--max-discards` if the journal shows the cap being hit.
- It does not stop new checkpoints from being created; that needs the
  permanent fix: a free-space guard on `Create`, a startup orphan sweep in
  hostd, and/or crash-loop backoff in filed. See the parent task for that
  follow-up.
