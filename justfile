# TemplarOS developer entry points. `just` (no args) lists recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]
export CARGO_TARGET_DIR := `scripts/native-target-dir.sh`

default:
    @just --list

# fmt + clippy -D warnings + tests + msrv + xtask checks
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    scripts/test-nonzero.sh --workspace
    just msrv
    just tree-check
    just deps-check
    just check-interfaces

test *ARGS:
    scripts/test-nonzero.sh --workspace {{ARGS}}

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# MSRV = workspace rust-version in Cargo.toml; the pinned development patch satisfies it.
msrv:
    cargo +"$(python3 -c 'import tomllib; print(tomllib.load(open("rust-toolchain.toml", "rb"))["toolchain"]["channel"])')" check --workspace
    cargo run -q -p xtask -- check-msrv

tree-check:
    cargo run -q -p xtask -- tree-check

deps-check:
    cargo run -q -p xtask -- deps-check

check-interfaces:
    cargo run -q -p xtask -- check-interfaces
    cargo run -q -p xtask -- check-interface-signatures
    cargo run -q -p xtask -- check-proxies
    cargo run -q -p xtask -- check-packaging
    cargo run -q -p xtask -- check-ownership
    cargo run -q -p xtask -- check-assets
    cargo run -q -p xtask -- check-actions
    cargo run -q -p xtask -- check-wayland-versions
    cargo run -q -p xtask -- check-logging

# `templar doctor` against this machine
doctor *ARGS:
    cargo run -q -p templar -- doctor {{ARGS}}

# Measure the shared file index's runtime cost (docs/measurements/): dir count/watch limits,
# then the timed-workload suite against a generated fixture. Idle-cost sampling and the
# opt-in 2,000,000-entry case are not part of this default recipe — see
# `cargo run -q -p xtask -- index-bench --help`.
index-bench:
    cargo run -q -p xtask -- index-bench dirs
    cargo run -q -p xtask -- index-bench workloads

# The provisioner: reviewed plan on sparse files, then the layout and refusal smoke
provision-test:
    tests/integration/provision-plan.sh
    tests/integration/provision-qemu.sh

# Hotfix the installed operating system (refuses Lubuntu and the live ISO).
update-os *args:
    ./scripts/update-templaros.sh {{args}}

# Stage-1 packages via cargo-deb (target/debian/)
preview-codecs:
    ./scripts/build-preview-codecs.sh

# One identified build set: every package in distro/deploy/components.toml at one version.
deb: preview-codecs
    cargo run -q -p xtask -- build-set

# What `just deb` would build, and under which id.
deb-plan:
    cargo run -q -p xtask -- build-set --dry-run

# Close workspace + preview-codec sibling sources/assets/licenses and locked vendor inputs.
source-archive:
    python3 scripts/build-local-source-deb.py workspace

# Prepare a NEW isolated tree from a retained trusted exact-source archive.
prepare-source archive sha256 destination:
    python3 scripts/build-local-source-deb.py workspace --source "{{archive}}" --expect-sha256 "{{sha256}}" --extract "{{destination}}"

# Historical entrypoint now closes the full source layout, not just registry crates.
vendor: source-archive

# Fetch/update upstream forks into forks/ (see distro/forks/manifest.toml)
forks-fetch:
    distro/forks/fetch.sh

# The installation medium: a live ISO carrying the archive's current set (distro/iso/build.sh).
# Needs a non-NTFS build volume (ISO-PLAN.md P0.2) and a published set (`just deb`, then
# `sudo templar dev repo publish`).
iso *args:
    ./distro/iso/build.sh {{args}}

# Compose mkosi package list from seeds, render os-release, validate (build needs mkosi + network)
image action="summary":
    ./distro/image/compose.sh
    mkosi --directory distro/image {{action}}
    if [ "{{action}}" = build ]; then if [ "${TEMPLAROS_READ_ONLY:-0}" = 1 ]; then sudo bash distro/image/finalize.sh --read-only distro/image/mkosi.output/templaros_0.1.raw; else sudo bash distro/image/finalize.sh distro/image/mkosi.output/templaros_0.1.raw; fi; fi

# mkosi without the seed compose: validating a mkosi.repart/*.conf change must not also
# regenerate the package list, which is a different concern and a different owner.
image-mkosi action="summary":
    mkosi --directory distro/image {{action}}
    if [ "{{action}}" = build ]; then if [ "${TEMPLAROS_READ_ONLY:-0}" = 1 ]; then sudo bash distro/image/finalize.sh --read-only distro/image/mkosi.output/templaros_0.1.raw; else sudo bash distro/image/finalize.sh distro/image/mkosi.output/templaros_0.1.raw; fi; fi

# Whole CI pipeline locally
ci:
    ci/run.sh

# Regenerate docs/generated from the AKR ledger
akr-build:
    akr build

# Every outstanding manual acceptance check, grouped into one post-login pass.
predeploy-checklist:
    ./scripts/predeploy-checklist.py
