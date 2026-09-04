# LegeOS developer entry points. `just` (no args) lists recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# fmt + clippy -D warnings + tests + msrv + xtask checks
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    just msrv
    just tree-check
    just deps-check
    just check-interfaces

test:
    cargo test --workspace

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt:
    cargo fmt --all

# MSRV = workspace rust-version in Cargo.toml; the pinned development patch satisfies it.
msrv:
    if rustup toolchain list | grep -q '^1.97'; then cargo +1.97 check --workspace; else echo "install with: rustup toolchain install 1.97"; fi

tree-check:
    cargo run -q -p xtask -- tree-check

deps-check:
    cargo run -q -p xtask -- deps-check

check-interfaces:
    cargo run -q -p xtask -- check-interfaces
    cargo run -q -p xtask -- check-interface-signatures
    cargo run -q -p xtask -- check-proxies
    cargo run -q -p xtask -- check-packaging
    cargo run -q -p xtask -- check-assets
    cargo run -q -p xtask -- check-actions

# `lege doctor` against this machine
doctor *ARGS:
    cargo run -q -p lege -- doctor {{ARGS}}

# Measure the shared file index's runtime cost (docs/measurements/): dir count/watch limits,
# then the timed-workload suite against a generated fixture. Idle-cost sampling and the
# opt-in 2,000,000-entry case are not part of this default recipe — see
# `cargo run -q -p xtask -- index-bench --help`.
index-bench:
    cargo run -q -p xtask -- index-bench dirs
    cargo run -q -p xtask -- index-bench workloads

# Stage-1 packages via cargo-deb (target/debian/)
preview-codecs:
    ./scripts/build-preview-codecs.sh

# One identified build set: every package in distro/deploy/components.toml at one version.
deb: preview-codecs
    cargo run -q -p xtask -- build-set

# What `just deb` would build, and under which id.
deb-plan:
    cargo run -q -p xtask -- build-set --dry-run

# Vendor dependencies for the offline archive build (debian/rules)
vendor:
    cargo vendor --locked vendor > /dev/null
    echo "vendor/ ready (ship it in the orig tarball)"

# Fetch/update upstream forks into forks/ (see distro/forks/manifest.toml)
forks-fetch:
    distro/forks/fetch.sh

# Compose mkosi package list from seeds, render os-release, validate (build needs mkosi + network)
image action="summary":
    ./distro/image/compose.sh
    mkosi --directory distro/image {{action}}

# Whole CI pipeline locally
ci:
    ci/run.sh

# Regenerate docs/generated from the AKR ledger
akr-build:
    akr build
