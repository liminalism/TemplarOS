# WP17 formatting and refreshed native gates — 2026-10-09

The required full workspace formatting gate now passes. `cargo fmt --all` changed
exactly **206 files**: 193 LegeOS and 13 sibling Pixelkit. All 610 pre-format Rust
sources, exact before/after hashes and a separate formatting patch are retained.
The earlier failed gate remains recorded as a historical observation.

Formatting exposed two overlong display functions. Their existing geometry
validation and duplicate-timing selection were extracted into helpers; error
ordering, size/scale limits, mode scoring and ties are preserved. Existing display
tests pass. A separate helper patch is retained.

## Executed gates

| Gate | Observation |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| Required Index1 all-target Clippy, `-D warnings` | Pass, no new suppressions |
| `cargo check --workspace --all-targets` | Pass |
| Eleven selected native test packages | **1,803 passed, 0 failed, 11 ignored**; 35 target summaries |
| Complete `just check-interfaces` recipe | All nine steps pass |
| Precise owned debug crashes | Scan, snapshot and fake-checkpoint delta journeys pass |
| Fresh coherent package set | **23 packages**, `0.1.0~dev1791505403+gebc54d2.dirty`; independent verification passes |

Tests own their bus and writable state. All **675 captured source inputs** match
after the run, crashes and package verification. Tests overlap earlier cohorts;
counts are not added. These are selected-package tests and lint, plus full
workspace formatting/compilation, not a whole-workspace CI claim.

The same native run passes all ten watchdog tests, including the owned child
that exits with status 70, no signal and no core flag. This establishes the
critical-pressure decision and exit mechanism without installed recovery or GPU
hotplug proof.

The crash harness observes a whole 5,002-row resnapshot, snapshot death at row
2,500 without an escaped partial descriptor, surviving sealed bytes and refused
old cursors. Delta replay uses a fake checkpoint provider and synthetic stream.

## Packages and source

The fresh [package report](index-packages-2026-10-08-wp17.md) records actual controls,
hashes, seven runtime stamps and packaged compositor/helper self-checks. All
**625 archived first-party Rust/TOML/lock files** match the native tree.
The retained archive SHA256 is `b7b5d0197ce38add31116d3305e17eb4b91bd90745250914adf1f675d445ff07`;
its content digest is `7d0c5ec33eaf28e4b588aedd348419ba25dded9542570e438fab299392d8cb4c`.
First-party packages use the shared native tree; the archive was not used for
an isolated workspace rebuild. LiminalView was rebuilt offline from its closed
archive. Nothing was installed and the headless checks exercise no DRM or HDMI.

## Remaining acceptance

WP0's missing historical permission reproduction, WP9's real-volume journey,
WP10's retired nested search GUI check and WP17's installed developer day remain
open. WP14's legacy comparison and WP16's FSearch adoption are owner-deferred
until the future own Index1 GUI with a large FSearch-style window. The WP8 writer
oracle remains. [Physical HDMI qualification](../architecture/hdmi-validation-handoff.md)
still requires an owner-controlled environment.

The [JSON](index-formatting-2026-10-09-wp17.json) retains exact commands, hashes,
source inputs and cohort limits. Ledger status is recorded in the closure report.
