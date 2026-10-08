`ci/run.sh` is the whole pipeline (called by `.github/workflows/ci.yml` and usable locally): fmt, clippy
`-D warnings`, tests, MSRV 1.97 check, cargo-deny (if installed), xtask checks, integration smoke, cargo-deb smoke.

Use `scripts/test-nonzero.sh` for focused Rust gates. It owns a private bus and disposable
HOME/XDG state, clears host displays, isolates `XDG_CONFIG_DIRS`, and directs system-bus access
to the fixture bus. Cargo success with zero executed tests is a failure. Native S00 smoke on
2026-10-06 deliberately misspelled an existing xtask binary test filter: Cargo reported zero
passed tests and the wrapper exited 1 with its nonzero-count diagnostic.

`xtask check-interfaces` and `check-proxies` audit production declarations using the same source
scope: explicit `#[cfg(test)]` items and Cargo's `tests/`, `examples/`, and `benches/` targets are
excluded. Exclusion ends at the test item's closing delimiter, so shipping declarations later
in the file remain audited; platform/feature-gated declarations remain in scope. A fixture
server cannot satisfy a shipping proxy, and a shipping declaration still requires XML.
`check-interface-signatures` independently audits all D-Bus XML against the frozen baseline;
test-source exclusion does not narrow that contract check.

Native source proof on 2026-10-06: the actual checker accepted undocumented test-only fixture
declarations, rejected a declaration outside their scope without XML, and rejected a shipping
proxy backed only by a test fixture. Temporary proof source was removed; no fixture XML was
added. The existing tree passed interface/proxy gates (41 interfaces), signature checking
(270 members, 22 additive members not yet frozen), tree-check (41 directories), and deps-check.
The disposable consumer regression and existing extraction/signature regressions also passed
through the private, nonzero-test wrapper. These focused source gates do not run the installed
desktop, deployment, or hardware portions of the whole CI pipeline.

Packaging audits unit executables against source assets and native Ubuntu package ownership.
A distribution-owned executable is accepted only when its `dpkg-query` owner is explicitly
in the dependencies of the package shipping that unit; another package's dependency does not
authorize it. Account recovery retains its private-mount `unshare` entrypoint and namespace
semantics; `util-linux` is already a declared hostd dependency.

For an optional workspace helper shipped inside another package, that package's
`package.metadata.deb.features` must enable the helper's required feature. Sessiond
ships the separate wallpaper decode executable with `templar-wallpaper/decode` in
its Debian build metadata; normal session-policy library builds retain empty defaults.

The 2026-10-06 native stabilization gate subsequently passed the full workspace
`--all-targets` suite: 2,442 executed tests with private bus and disposable HOME/XDG
state. Shared sibling PixelKit raster/shell/UI/windowing consumers separately
passed 345 tests with accessibility enabled. These are observed regression counts,
not fixed catalogue sizes or installed/physical acceptance. The whole-workspace
format and warning-denying clippy checks reported outstanding formatting and static-lint
diagnostics across the working tree, including concurrent wallpaper changes. These
focused passing gates do not establish a clean full CI run.

`ci/source-build.sh` builds from an explicitly pinned complete source closure in a disposable
Ubuntu 26.04 root (`mmdebstrap`, else `podman`). Set `TEMPLAROS_WORKSPACE_SOURCE_ARCHIVE`
and `TEMPLAROS_WORKSPACE_SOURCE_SHA256` after the `just source-archive` preparation described
in `CONTRIBUTING.md`; a Git HEAD copy plus crates.io-only vendoring is not a sibling-source recipe.
Identity and changelog are generated inside the guest with one recorded
`SOURCE_DATE_EPOCH`; an extracted closure gets `+s<full-bundle-content-sha256>`,
not a fictional commit or the host's HEAD. Git-origin ids remain valid and the
shared parser preserves strict cohort/version matching.
The host needs Python 3.11+ and a usable backend, not a cached host xtask build.
The guest installs the declared patch-pinned rustup toolchain and every non-Rust
Build-Depends, including the compositor's native input/seat/graphics libraries.
The locked `xkbcommon` graph links the base development library with its Wayland
feature. Its X11 transport uses `xkbcommon-dl`, so the X11 prerequisite is the
runtime `libxkbcommon-x11-0`, rather than unused X11 development headers.
Approved guest network access is required for OS packages/compiler preparation;
Cargo registry/Git sources are offline and both tool homes are guest-local.
The existing independently checked native-toolchain convention fulfills only
the Rust package prerequisites; `dpkg-checkbuilddeps` still gates all other
dependencies before the real `debian/rules binary` sequence. No dummy Rust
packages, `dpkg-buildpackage -d`, or distro-version fallback are used.
The root rules require matching content identity/changelog and stage both the
session-owned native argv adapter and wallpaper decoder, plus the PDF companion.
Successful `.deb` files and source/compiler provenance survive disposal in
`target/source-build/<archive-sha256>/<epoch>/`; a reused output is refused.
No clean-root build is claimed by source closure alone. With neither backend
installed it prints `NOT EXECUTED` and exits 0; that line is never a pass.
`--plan` prints a plan without compiling tools. `CONTRIBUTING.md` gives the pinned
invocation and distinguishes the retained, unvendored standalone effects sandbox
from the root/PDF graphs actually closed by this package lane.
Run `PYTHONDONTWRITEBYTECODE=1 python3 tests/integration/source-closure.py` for pure
archive pin/asset/refusal/relocation and native-prerequisite-filter regressions;
these do not compile software. Rust source-origin id/changelog and fixed-epoch
xtask tests, extracted metadata, PDF self-check and native packaging are separate
execution gates.
