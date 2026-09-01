# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project knowledge lives in AKR, not in Markdown

`AGENTS.md` is the binding protocol for this repository — read it. Durable decisions,
plans, observations and evidence live in `.akr/` and are reached through the `knowledge.*`
MCP tools (`knowledge.start` / `knowledge.context` before a task, `knowledge.validate`
before handing back). `docs/generated/` is build output rendered from that ledger: never
edit it, never hand-edit a `.akr` file, never read `.akr/cache/`. Working notes go in
`.agent/scratch/` (persistent — prune it before handing back).

## Commands

```sh
just                # list recipes
just check          # fmt --check + clippy -D warnings + tests + MSRV + all xtask checks
just test           # cargo test --workspace
just fmt / just lint
just ci             # ci/run.sh — the whole CI pipeline locally (adds cargo-deny, smoke, deb)
cargo test -p lege-field --test field name_of_test   # a single test
```

Repository-specific gates (all run by `just check`; each is also a standalone recipe):

```sh
cargo run -q -p xtask -- tree-check         # directory layout matches ARCHITECTURE.md
cargo run -q -p xtask -- deps-check         # e.g. lege-window/lege-field stay free of tokio/zbus
cargo run -q -p xtask -- check-interfaces   # code matches interfaces/ (the contract source of truth)
cargo run -q -p xtask -- check-proxies      # lege-client proxies match the D-Bus XML
cargo run -q -p xtask -- check-packaging    # debian/ + distro/ package facts
cargo run -q -p xtask -- check-assets
cargo run -q -p xtask -- check-actions
just msrv                                   # cargo +1.93 check --workspace
```

Running things:

```sh
cargo run -p lege -- doctor --root /        # the one integration checker (runtime/image/hardware)
cargo run -p lege-sessiond -- --dry-run
cargo run -p lege-resourced -- --once
cargo run -p lege-comp -- --check           # construct the compositor, report, exit (no display)
cargo run -p lege-comp -- --backend winit   # nested compositor; udev is the default on a bare TTY
tests/integration/run.sh                    # smoke: builds, then invokes the built binaries

# Working on the shell without logging out of a working desktop:
scripts/nested-session.sh start             # compositor + sessiond + filed + shell, nested
scripts/nested-session.sh shell             # rebuild and restart just the shell (500ms hot swap)
scripts/nested-session.sh launch <app-id>   # exercise the real launch chain
scripts/nested-session.sh logs              # every process's output, in files
cargo run -p lege-shell -- --render out.png # the Home Field as a PNG, no compositor needed
just deb                                    # cargo-deb packages into target/debian/
scripts/reinstall-legeos-desktop.sh         # build + reinstall + reload the desktop packages on this host
```

## Toolchain

Development is on Rust **1.97.1** (`rust-toolchain.toml`); the workspace MSRV is **1.97**
(`rust-version` in `Cargo.toml`), enforced by `just msrv` and CI (ADR-0008 raised it from
Ubuntu 26.04's 1.93 because the resolved dependency graph requires 1.97).
Edition 2024. Clippy runs `pedantic` workspace-wide with `-D warnings`.
`#![forbid(unsafe_code)]` unless the crate documents why not.

Per user convention, build two profiles when testing vs shipping: debug-release (release
opts, LTO off) for day-to-day, release-final (full LTO) for release artifacts. Keep
`cargo fmt --all` in its own commit, never mixed with logic.

## Architecture

`ARCHITECTURE.md` has the full map; `README.md` states the goals. The shape that matters:

- **One policy daemon.** `lege-sessiond` owns identity/launch, associations, providers,
  history, notifications, settings, projects, power, location — one state DB, one crash
  unit (ADR-0001). `lege-resourced` is deliberately separate and protected (no
  SQLite/fsync/D-Bus on its loop, ADR-0002); `lege-filed` is separate for heavy I/O;
  `lege-hostd` is the single privileged system-bus surface (ADR-0003).
- **Pure crates, thin hosts.** `crates/` holds the testable cores; `services/`, `shell/`,
  `apps/`, `portals/`, `cli/` are hosts over them. `lege-window` (geometry invariant,
  property-tested) and `lege-field` (Home Field model: ops with exact inverses, reflow,
  drag/throw) are toolkit-free and async-free by rule.
- **Compositor vs shell.** `lege-comp` (Smithay, GPL-3.0, ADR-0012) is conservative and
  minimal; `lege-shell` is everything decorative and restartable. `compositor/COMPOSITOR.md`
  is the binding contract: one workspace, `lege-window` as the sole z-order and visibility
  authority, compositor-drawn recovery band, private `lege_shell_v1` protocol.
- **`interfaces/` is the single source of truth** for D-Bus XML, Wayland protocols and JSON
  schemas. Change it *first*, then the code; shipped signatures are frozen — bump the
  interface version instead.

Key invariants the code enforces (see ARCHITECTURE.md for the full list): identity comes
from the cgroup, never the client's `app_id`; `is_recoverable(window)` holds after every
drag/output change; detected MIME beats extension unless the user overrode it; exactly one
provider per exclusive role autostarts; nothing under `/usr/local` — every file is owned by
a Debian package.

## Gotchas

- **Sibling-repo path dependencies.** `pixelkit-*`, `bpg-decode` and the preview codecs
  resolve to sibling directories outside this repo (`../pixelkit`, `../bpg-rs`,
  `../jpegXL-rs`, `../Lege-ecosystem`). A missing sibling breaks the workspace build. These
  must become pinned git deps before packaging.
- **Preview codecs are a separate toolchain.** `helpers/lege-preview-codecs` builds under
  Rust 1.97 via `scripts/build-preview-codecs.sh` (`just preview-codecs`, a prerequisite of
  `just deb`) to keep its heavier dependency graph out of the service build. Its lockfile drifts when
  sibling repos change deps; refresh with `cargo +1.97.1 update --workspace --offline`.
- **Forks are never vendored.** Add to `distro/forks/manifest.toml`, keep `upstream` and
  `legeos` branches (`just forks-fetch`).
- **The compositor is the one thing that cannot be hot-swapped.** `lege-comp.service` is
  `Restart=no` by design (a crash disconnects every client, so there is nothing to restart
  into); everything above it restarts in about half a second. Compositor changes need a
  logout, so prefer `scripts/nested-session.sh` while iterating.
- **Two shell tests describe the no-service path** and skip when `org.legeos.Session` is
  answering — which is what running the nested session does. A skip there is not a failure.
- **`lege doctor` reports design mismatches on a non-LegeOS dev host** (os-release ID,
  MIME handler comparison). A non-zero exit there is not necessarily a real fault.
- Commits: one concern each; reference the ADR when a decision changes; log friction with
  `knowledge.papercut` as you hit it.
