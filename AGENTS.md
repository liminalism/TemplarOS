# Memory

Your memory is OptMem. TemplarOS is developed and validated from the machine's native
Lubuntu installation; WSL is only an agent-side helper when a request starts from
Windows and must not be treated as the TemplarOS build environment.

- On Lubuntu, the tool is `~/.optmem/memo` and its storage is
  `~/.optmem/memory`.
- From a Windows-hosted agent session, invoke the helper store with
  `wsl.exe bash -lc '~/.optmem/memo <command>'`; do not run project builds or
  validation there.

OptMem outlives every session, compaction, model and vendor change.

## At startup: activating OptMem (mandatory)

Run `~/.optmem/memo wake` before any other tool call in a Lubuntu session. From a
Windows-hosted agent session, use
`wsl.exe bash -lc '~/.optmem/memo wake'`. Follow its output to the end.

## While working: register memories (mandatory)

Run `~/.optmem/memo note "<1 line, max 280 bytes>"` whenever you learn a durable,
non-redundant fact or complete meaningful work. Use the WSL invocation above from
Windows. If `note` asks for a compression, finish it before the next action. Never
edit or delete anything under `~/.optmem/memory`; the tool owns that storage.

Use `memo recall <regex>` to search raw memories and `memo zoom <lo>-<hi>` to
expand a summary node.

Subagents must not run OptMem. When spawning one, state: `You are a subagent.
Don't run memo.`

Subagents never run tests; parent does.

# Agent protocol

## Project knowledge (AKR)

Durable project knowledge lives in `.akr/` as typed records, not in Markdown.
`docs/generated/` is build output. Follow this protocol.

**Before starting any task**
1. If you know the exact planning key, call `knowledge.context` with that key and the
   paths you expect to touch (shell: `akr context --goal <key>` — `--goal` is required;
   a positional key fails with AKR-C003).
2. Otherwise call `knowledge.start` with the task and paths. Read its session head, pick
   a live candidate (or an explicitly relevant proposal), then call `knowledge.context`
   with that exact key.
3. Read context bundles in full. Contradictions and staleness warnings are always
   included and are never noise.

**While working**
- Look things up with `knowledge.get`; find them with `knowledge.search`.
  Search ranks results; it never grants authority. A record's standing comes from its
  state, its scope, and its relations.
- Scratch notes go in `.agent/scratch/`. Nobody reviews them and nothing depends on them.

**When something becomes durable**
- New knowledge: `knowledge.propose`. Observations need `observed_at` and, if they can
  go out of date, `watches`.
- Changed knowledge: `knowledge.revise`. Never edit a `.akr` file directly, and never
  edit a record that is not `proposed`.
- Replacing a plan: `knowledge.supersede`, with a disposition for every unfinished
  child. The tool will list them; answer each one.
- Finishing work: record what you observed with `knowledge.evidence_add`, then
  `knowledge.complete` with evidence for every acceptance check. Evidence records
  state what was observed; they never state what they verify.
- Unsure what a kind requires? `akr explain <kind>` prints its schema.

**Papercuts**
- When you hit a small friction while working — a tool call that missed and had to be
  retried, a confusing or undocumented setup step, a flaky command, a stale cache, a
  misleading error, a non-obvious gotcha — log it with `knowledge.papercut` (or
  `akr papercut -m <agent> "message"`). One or two sentences: what you were doing,
  what got in the way (a guess at the cause/fix is a bonus). Do this proactively, in
  the moment, even though none of these are blocking — logged together they show where
  the project needs sanding down. This is distinct from durable records (knowledge) and
  from `.agent/scratch/` (working notes).

**Never**
- Never touch the person's live desktop session. That means no `systemctl --user`
  start/stop/restart/kill of any `templar-*` unit, no `loginctl`, no `pkill`/`killall` by
  a name that can match an installed `templar-*` binary (`pkill -f templar-sessiond` matches
  `/usr/libexec/templar-sessiond` and once ended the person's session), no `templar dev
  deploy` (dry runs excepted), and no development service started on the live user bus
  while the installed one owns its name. Test with `cargo test`, the headless renders
  (`templar-shell --render`, `templar-comp --check`) and `scripts/nested-session.sh`, which
  refuses to start over a live session. Deploys and logouts are the person's to run.
- Never edit `docs/generated/` — it is regenerated and CI checks it.
- Never read `.akr/cache/` — it is a private cache.
- Never delete a record. Move it to a terminal state instead.

**Before handing back**
- `knowledge.validate`. If it reports diagnostics, fix them or say so explicitly.
