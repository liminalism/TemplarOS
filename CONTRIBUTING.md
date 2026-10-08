# Contributing

* **Read `AGENTS.md`** (project knowledge protocol) and `docs/adr/` before proposing structure changes.
* **Every component is a Debian package.** No `/usr/local`, no untracked home-directory state.
* **Forks**: never vendored. Add to `distro/forks/manifest.toml`; keep `upstream` and `templaros` branches; rebase
  monthly; patches must be explainable in one sentence each.
* **Interfaces**: change `interfaces/` first; `cargo xtask check-interfaces` must pass. Shipped signatures are
  frozen — bump the interface version.
* **`templar-window` stays free of tokio/zbus** (`cargo xtask deps-check`).
* **Tests**: policy engines are pure and unit-tested; geometry is property-tested; `templar doctor` is the one
  integration checker (runtime, image, hardware).
* **Style**: `just check` (fmt, clippy `-D warnings`, tests, MSRV 1.97). Edition 2024, AGPL-3.0-only header not
  required (licence is repository-wide), `#![forbid(unsafe_code)]` unless a crate documents why not.
* **Commits**: one concern per commit; reference the ADR when a decision changes; AKR records for durable
  knowledge (`knowledge.propose` / `revise`), papercuts for friction.

**Naming:** TemplarOS is the operating-system product; LegeOS is the retained repository
directory and historical `legeos.*` AKR namespace. Package names, service/API names and
current user-facing documentation use TemplarOS/`templar`/`templaros` as their contracts
require. Do not bulk-rename historical records, source paths or frozen interfaces.

`just msrv` reads the patch-pinned minimum compiler from `rust-toolchain.toml` using Python 3.11+
and runs the check explicitly with that toolchain; a missing compiler fails the gate.

Native builds use `target/native/<os-release ID>-<VERSION_ID>` to avoid reusing system-library
links from another OS mounted on the same checkout. `just` sets this automatically; for direct
Cargo commands use `export CARGO_TARGET_DIR="$(scripts/native-target-dir.sh)"` once in your
shell, then `cargo build -p templar-shell`, or use `scripts/cargo-native.sh build -p templar-shell`.
An explicit `CARGO_TARGET_DIR` is never overridden. `debian/rules` retains its explicit `target/`
for `.install` paths; cargo-deb honors the native target for binary assets, while build-set output
remains in `target/debian/`.
The preview helper also keeps its Cargo cache under the native OS target; only its packaging
staging files remain in `target/preview-codecs/`. Give each Git worktree its own target directory:
sharing build fingerprints between checkouts can reuse stale metadata even on the same OS.

The native Rust archive path requires `libarchive-dev` (including its pkg-config metadata)
and `libclang-dev`. The maintained network fork additionally needs `cmake`, `qt6-base-dev`,
`qt6-tools-dev`, `qt6-tools-dev-tools` and `qt6-l10n-tools`: the tools-only packages do not
provide `Qt6LinguistToolsConfig.cmake`. Its Debian overlay also requires `debhelper`.
Install these on the native OS, not on a mounted Lubuntu/WSL root.
`scripts/reinstall-templaros-desktop.sh --skip-build` reuses `target/debian/` packages,
including the maintained network fork; deployment and logout remain the owner's action.

## Exact-source preparation

The root workspace and `helpers/templar-preview-codecs` have separate lockfiles
and consume owner-supplied sibling trees. A bare checkout is not the complete
build input. Do **not** reset those siblings to their Git HEADs: their dirty
content, embedded fonts, native codec source and license notices are inputs.
Declared repository URLs are not evidence of a release origin or an exact pin.

`just source-archive` extends the existing local source closure tool used for
LiminalView; it closes the **current locked** workspace and preview graph into
`target/source-archives/templaros-workspace-SHA256.tar.gz`. It copies local package
trees/assets, owning manifests and licenses (including nested `license-file`
notices), and vendors both graphs' locked registry/Git dependencies offline.
The root repository's build scripts, interfaces, packaging and static assets are
retained too. Private assistant/cache directories and build outputs are excluded.
The tool records per-root exact content hashes, Git HEAD/dirty observations,
dependency licenses, both lock hashes and a whole-bundle content hash in the
existing `source-provenance.json` format. Git observations are not source pins.
It validates both graphs against an empty Cargo home before retaining the archive.
The provenance names the two `closed_manifests` explicitly. Independent,
non-packaged workspaces such as `tools/effects-sandbox` retain their current
source, shaders and lockfile inside the content hash, but their graphs are **not**
vendored by this package lane. `retained_unvendored_workspaces` records those
manifests and lock hashes; it is not an offline-build claim for the sandbox.
This is source closure, **not** evidence that compilation or packaging passed.

Missing locked downloads fail; an owner-authorized connected preparation can
first run `cargo fetch --locked` and
`cargo fetch --locked --manifest-path helpers/templar-preview-codecs/Cargo.toml`.
Stale locks also fail: production never implicitly updates them. Only while
authoring an intentional dependency change, refresh the affected lock explicitly
(`cargo update --workspace --offline`, with the helper's `--manifest-path` when
needed), review it, then close a new archive. The separate preview builder now
enforces `--locked` from its first metadata request.

Retain the archive and its printed SHA256 through the owner's approved source
storage. These commands do **not** upload or publish anything. A public fresh
checkout still requires owner-approved exact archives or genuinely supplied
release origins/pins and distribution/license review for every local input.
In particular, PixelKit/PDF renderer manifests do not supply an exact public
release pin. Do not infer permission to publish those trees from this local lane.

For a fresh developer build, use a trusted recorded archive hash and a
**nonexistent** destination; preparation never overlays a sibling or checkout:

```sh
python3 scripts/build-local-source-deb.py workspace \
  --source /approved/templaros-workspace-SHA256.tar.gz \
  --expect-sha256 SHA256 --extract /var/tmp/templaros-exact-source
cd /var/tmp/templaros-exact-source/sources/LegeOS
export CARGO_HOME=/var/tmp/templaros-exact-source/isolated-cargo-home
export CARGO_NET_OFFLINE=true
export CARGO_TARGET_DIR=/var/tmp/templaros-exact-source/build-target
cargo metadata --locked --offline --format-version 1
cargo metadata --locked --offline --format-version 1 \
  --manifest-path helpers/templar-preview-codecs/Cargo.toml
just deb
```

`LegeOS` above is the recorded `application` field, not a forced directory name.
Prepared Cargo configuration uses the retained vendor tree at `../../vendor`;
the complete directory remains relocatable. Rust **1.97.1**, rustup, cargo-deb,
just, Python 3.11+ and the declared native Debian build libraries remain real
host prerequisites, not included system binaries or fake source fallbacks.
The empty Cargo home/explicit target prevent host caches from satisfying omissions.
The source lane `ci/source-build.sh` accepts this same pinned archive through
`TEMPLAROS_WORKSPACE_SOURCE_ARCHIVE` and `TEMPLAROS_WORKSPACE_SOURCE_SHA256`.
The host needs Python 3.11+ and either a privileged mmdebstrap/chroot backend or
a usable podman backend; it does not compile xtask or consult host Cargo caches.
Scratch defaults to `target/native/ubuntu-26.04/source-builds` on the target
filesystem, not `/tmp`; an explicitly supplied `TMPDIR` remains honored.
The disposable Ubuntu 26.04 guest needs approved network access for its OS
packages and the patch-pinned rustup compiler. It installs every non-Rust
`debian/control` Build-Depends through `mk-build-deps`, then independently checks
the real native compiler against both locked graphs and checks all those Debian
dependencies with `dpkg-checkbuilddeps`. As with the existing local native source
builder, checked Rustup binaries fulfill the Rust toolchain prerequisites:
`debian/rules binary` runs the real debhelper binary-package sequence, without
`dpkg-buildpackage -d`, dummy Rust packages or a distro-compiler fallback.
Cargo registry/Git access stays offline and Cargo/Rustup homes are guest-local.
This lane is not qualified by archive preparation.

For a clean-root build, retain the exact archive and record one epoch:

```sh
export TEMPLAROS_WORKSPACE_SOURCE_ARCHIVE=/approved/templaros-workspace-SHA256.tar.gz
export TEMPLAROS_WORKSPACE_SOURCE_SHA256=SHA256
export SOURCE_DATE_EPOCH=RECORDED_UNSIGNED_EPOCH
ci/source-build.sh
```

The lane creates the id and changelog **inside the guest**, passing this epoch to
both xtask calls. Extracted source uses the existing BuildSetId authority with
the distinct origin `0.1.0~dev<epoch>+s<full-bundle-content-sha256>`, never a
fabricated `g0000000` commit or the host's HEAD. Git checkout ids retain their
existing `+g<sha7>[.dirty]` form; all consumers use the same strict parser and
cohort comparisons remain exact string equality. A snapshot's observed dirty
Git inputs are recorded in provenance, not substituted for its content identity.
Runtime stamps for a supplied source-origin id leave the unavailable Git SHA
empty rather than borrowing an enclosing checkout or asserting a fictional commit.
`debian/rules` rejects missing ids, mismatched changelogs and source-origin hashes
that disagree with the closure. It builds the preview helper before filed and
requires the session-owned decoder and native `templar-app-exec` assets.
Successful output is retained under
`target/source-build/<archive-sha256>/<epoch>/`, including `.deb` files, the
build-set id, exact source provenance, archive pin and checked compiler/dependency
facts. An existing output directory is refused, not mixed with another run.

LiminalView is a **separate** selected application's already-retained closure,
not an interchangeable root/preview archive. Its native build-set sidecar records
`liminalview-44c21a68851687a406e05cc2b19fa904f1b71f9788f4a48748844669902700b1.tar.gz`,
SHA256 `44c21a68851687a406e05cc2b19fa904f1b71f9788f4a48748844669902700b1`,
bundle content `692474af633e06bfd5c13179c57ee3fb3d1e8c62bda068adc600650b4b95a9cd`.
It has no supplied public Git origin/pin. To include that exact application in a
fresh complete set, pass its retained archive via
`TEMPLAROS_SOURCE_ARCHIVE_LIMINALVIEW` (see `distro/forks/README.md`).
Do not silently choose the newest archive or reconstruct its dirty inputs from HEAD.

Source regressions to run after integration:
`PYTHONDONTWRITEBYTECODE=1 python3 tests/integration/source-closure.py`.
They cover stale outer pins, omitted embedded assets, existing-checkout refusal,
relocatable sibling/vendor layout and the narrowly checked Rust prerequisite
substitution. Rust identity regressions additionally cover the source-origin
parser/changelog and fixed-epoch xtask identity. Fresh extracted metadata,
preview self-test and the complete native package build remain separate gates.
