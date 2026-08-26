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

# MSRV = Ubuntu 26.04's rustc (rust-version in Cargo.toml)
msrv:
    if rustup toolchain list | grep -q '^1.93'; then cargo +1.93 check --workspace; else echo "install with: rustup toolchain install 1.93"; fi

tree-check:
    cargo run -q -p xtask -- tree-check

deps-check:
    cargo run -q -p xtask -- deps-check

check-interfaces:
    cargo run -q -p xtask -- check-interfaces
    cargo run -q -p xtask -- check-proxies
    cargo run -q -p xtask -- check-packaging
    cargo run -q -p xtask -- check-assets
    cargo run -q -p xtask -- check-actions

# `lege doctor` against this machine
doctor *ARGS:
    cargo run -q -p lege -- doctor {{ARGS}}

# Stage-1 packages via cargo-deb (target/debian/)
preview-codecs:
    ./scripts/build-preview-codecs.sh

deb: preview-codecs
    for p in lege-sessiond lege-resourced lege-filed lege-hostd lege-boot-health lege-session lege-comp lege-greeter lege-shell lege-settings lege-files xdg-desktop-portal-lege lege; do cargo deb -p "$p" -q; done
    ls -1 target/debian/*.deb

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
