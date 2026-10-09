# WP17 current native closure checks — 2026-10-08

## October 9 formatting and refreshed gates

Full workspace formatting now passes after correcting 206 files (193 LegeOS,
13 sibling Pixelkit). The formatter patch and 610 original Rust sources are
retained separately from the two display helper extractions. Required Index1
all-target Clippy and whole-workspace all-target compilation also pass.

The eleven-package native private run passes **1,803 tests, zero failures and
11 ignored** across 35 target summaries, including display tests. The full
interface recipe and all three precise owned debug crash journeys pass again;
the checkpoint provider remains fake. All 675 captured source inputs match.
Counts overlap previous runs and are not added.

The fresh [23-package set](index-packages-2026-10-08-wp17.md) is
`0.1.0~dev1791505403+gebc54d2.dirty`. Controls, hashes, seven runtime stamps and packaged headless
self-checks pass; all 625 archived first-party Rust/TOML/lock files match.
The retained workspace archive is not an isolated workspace rebuild. No
installation, real-volume, installed developer-day or physical HDMI claim.

See the [formatting cohort](index-formatting-2026-10-09-wp17.md) for exact scope,
commands, source and artifact hashes. WP17 retains two of three satisfied checks;
the owner-run day remains open, and the WP8 writer oracle remains. The owner
deferred WP14's legacy comparison and WP16's FSearch adoption for the future own
Index1 GUI. WP0's historical permission reproduction and WP10's retired nested
search-view acceptance still need explicit reconciliation.

The regenerated ledger validates: **1,906 revisions, 1,447 heads, zero errors and
32 warnings**. There are 151 dirty watched paths and 31 unmatched scopes. The
review queue retains 39 stale and 17 at-risk records. WP17 revision 9 is active
with two of three checks satisfied; the canonical parent still has 12 of 18 WPs
completed. The ledger evidence snapshot and final handoff snapshot are retained
separately.

## Earlier post-lint required-gate continuation

After the catalogue/client/filed lint refactors, the native ten-package private-bus
run passes **1,757 tests, zero failures and 11 ignored** across 33 target summaries.
All 623 recorded LegeOS Rust/TOML/lock hashes stayed unchanged during the run.
These counts overlap the earlier cohorts and are not added to them.

The plan's required core/filed/client **all-target Clippy passes with `-D warnings`**,
without new lint suppressions. Whole-workspace all-target compilation and the
complete interface recipe also pass. The formatter still reports differences in
**206 files**: 193 in LegeOS and 13 in sibling Pixelkit (208 path strings include
two aliases). All 193 LegeOS files also need formatting in tracked HEAD. That gate remains
open; this is not a full-workspace CI pass. Exact logs and hashes are retained in
the JSON's `post_lint_required_gates` observation.

The fresh [23-package set](index-packages-2026-10-08-wp17.md) at
`0.1.0~dev1791502518+gebc54d2.dirty` passes controls/hash/stamp and packaged self-checks;
all 625 archived first-party source hashes match. All three precise owned debug
crash journeys also pass again, with the fake checkpoint boundary explicit.
The earlier idle/crash/package observations below retain their original cohorts.
Installed developer-day, real-volume and physical HDMI gates remain open.
The WP8 writer oracle is retained.

The final regenerated ledger validates: **1,890 revisions, 1,432 heads, zero
errors and 32 warnings** (59 dirty watched paths and 31 unmatched scopes).
The review queue retains 39 stale and 17 at-risk records. WP17 revision 8
keeps the original acceptance statements and two of three checks satisfied;
the owner-run installed day is still open.

## Earlier observations

Source HEAD `12295f351ab14e649f900573190060cd88a7251e` plus the shared working
tree. Exact source hashes and command results are in the [JSON](index-closure-2026-10-08-wp17.json).

## CLI journal and native regressions

`cargo test -p templar -p templar-hostd -p xtask`, through the native capped build
wrapper, exited zero: CLI 236, hostd 109 and xtask 64, **409 passed, zero failed**.
The earlier index/filed/client/treemap/shell/Settings run passed 1,074 with 11
ignored; these are separate scoped runs, not a workspace-wide claim.

The journal test's `bash --rcfile` still loaded system `bash.bashrc`. This host's
installed garden hook could satisfy the idempotence guard before the checked-out
hook was sourced. The fixture now uses `--noprofile --norc`, a clean environment,
an owned HOME/working directory and an explicitly sourced owned rc. On failure
it retains shell stderr in the assertion. Production hooks were not changed.

`python3 tests/integration/garden-session-hooks.py --shell bash` also exited zero
for the three history modes and actual installed Kitty integration: privacy,
pipeline status/vector, earlier hooks/DEBUG/extdebug, PS0 and command execution.
All shell sessions and files were owned fixtures; no live terminal was driven.

## Ownership provenance scope

The current production `real_config` reads `DeploymentLayout`; the mounted
candidate path and the Btrfs boot subvolume remain separate values. The current
native factory test changes the installed layout, including ESP and candidate
mount, and checks both the factory and engine. Layout, GRUB/systemd-boot entry,
transaction, account pairing and xtask declaration tests pass in the run above.

The old observation watched all hostd sources at an earlier Git base, so the
later integration commit made its support of the ownership decision stale.
Reconciliation must use these current source/fixture observations and preserve
the distinction from the earlier owned guest boot/rollback evidence. That guest
journey was **not rerun here**. Installed/reference hardware, physical HDMI,
Secure Boot and the full native writer/promotion day remain open.

## Owner search amendment

The owner deferred WP14's old shell/FSearch same-query generation comparison and
selected a future own integrated Index1 GUI with a large FSearch-style window.
The FSearch fork/package evidence remains a qualified prototype. WP16's fork
shipping/default adoption is deferred; no retired native Files route was revived.

## Reconciled interface recipe

The complete `just check-interfaces` recipe now exits zero: 44 server interfaces,
305 unchanged shipped members, 42 proxies, 14 packages/components, 15 seeds and
8 metapackages. Ownership checks 14 components, 5 subvolumes and 32 path domains.
Assets, 79 actions/21 chords, 46 versioned Wayland members and 12 binary logging
checks pass. The unchanged ownership policy now rests on observation revision5,
with the current fixture evidence and historical guest limits above. The gate
was neither disabled nor relaxed. Log: `.agent/scratch/index-wp17/interface-gates-reconciled.log`.

## Earlier private regressions and lint

The native ten-package run through `scripts/test-nonzero.sh` passes **1,505 tests**,
zero failures and 11 ignored, on a private bus with disposable HOME/XDG state.
It includes the 13 volume cases and the wallpaper selector/quantizer regressions.
The later Settings identity-confirmation change passes all **75 Settings tests**;
that run overlaps the earlier cohort and is not added to its total.

The final repeat with that Settings change included passes **1,506 tests**, zero
failures and 11 ignored. Both cohorts and source hashes are retained in the JSON.

The later acceptance audit strengthens four test sources and passes **384
core/filed tests**, zero failures and 10 ignored. It establishes four cases of
exactly 1,000 publication commits, all four 512-case parity runs, inaccessible
cache-parent byte preservation and production nested-root snapshot decoding.
The current index/wallpaper all-target Clippy command passes again. These scoped
counts overlap the earlier cohort and do not create a new whole-workspace total.
See the [audit](index-qualification-audit-2026-10-08.md) for exact source/log hashes.

`cargo clippy -p templar-file-index -p templar-wallpaper --all-targets -- -D warnings`
passes. Fixes include explicit container/type names, checked time subtraction,
equivalent loops and smaller wallpaper selector/mapping functions. Exact dyadic
float assertions keep their equality checks with narrowly explained allowances.

The earlier wider scoped lint run stopped at six diagnostics in
`crates/templar-session-core/src/session_catalogue.rs` (unit expression,
function length, nested condition and clone assignment). Those diagnostics are resolved by the latest required-gate continuation
above. That scoped result is not a full-workspace CI pass. Two sibling still265 deprecations remain.

The final full interface recipe passes again after relocation changes. Whole
workspace `cargo fmt --all -- --check` reports differences in 213 files, including
shared sibling sources. That gate remains open; no broad formatting rewrite was
applied. Exact paths and logs are retained in the JSON.

## Coherent native artifacts

The native build now succeeds for **23 packages** at
`0.1.0~dev1791495062+g12295f3.dirty`. Actual package controls, sizes and hashes
match the manifest, seven runtime-stamped binaries contain its ID, and the
packaged compositor headless check and preview-helper self-check pass.
See [the package cohort](index-packages-2026-10-08-wp17.md) for source closure
and the distinction between shared-tree and isolated local-application builds.
Nothing was installed; the owner-run developer day and physical gates stay open.

The later test-only changes are recorded separately from the package source
archive; no rebuilt runtime package or installation is claimed for them.

The complete owned [two-million-entry filesystem workload](index-scale-2026-10-08-wp17.md)
passes in a disclosed 4 GiB KVM/ext4 guest: complete scan in 37.07 seconds and
array-backed name-search p95 13–25 ms. SQL timings, earlier inconclusive attempts
and the absence of a two-million-row snapshot-memory measurement remain explicit.

The later [Silvaview action continuation](index-silvaview-2026-10-08-offline-actions.md)
passes 48 feature-on and 32 feature-off tests and the refreshed private worker
journey. Those counts are separate from the first-party 1,506-test cohort.

## Ledger handoff

`akr build` regenerated the seven views and lock. `akr validate` exited zero:
1,870 revisions, 1,414 heads, no errors and **32 warnings**. One warning covers
54 dirty watched paths; 31 are unmatched scopes, including untracked new work
and older moved/deleted paths. The review queue retains 39 stale and 17 at-risk
records. Exact logs and counts are in the JSON. These diagnostics remain visible.

The canonical parent now contains all 18 WP entries; WP1–WP8,
WP11–WP13 and WP15 are completed. Commit `ebc54d2` contains exactly the six
retained before-change benchmark reports. All 61 source hashes from the latest
core/filed run still match; no runtime package was rebuilt for this documentation
commit. Historical baseline, real-volume,
retired nested-search and installed checks remain open; WP14/WP16 are deferred
by the owner for the future integrated GUI.

WP17 revision7 has two satisfied checks: owned crashes and native closure
gates/docs. Its installed developer-day check remains open. The Silvaview
offline-action continuation is completed separately. The WP8 oracle stays until
installed closure; source/artifact results do not close the physical HDMI gate.
