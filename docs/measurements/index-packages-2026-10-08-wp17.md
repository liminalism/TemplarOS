# WP17 native package set — October 9, 2026

Prepared and inspected **23 packages** at `0.1.0~dev1791505403+gebc54d2.dirty` after workspace formatting
and the display helper extraction. The capped native build and independent
checker exit zero. Exact hashes and earlier cohorts are retained in the
[JSON](index-packages-2026-10-08-wp17.json).

```sh
CARGO_BUILD_JOBS=2 bash .agent/scratch/index-wp0/capped.sh \
  bash .agent/scratch/index-wp17/build-post-format-packages.sh
CARGO_BUILD_JOBS=2 bash .agent/scratch/index-wp0/capped.sh \
  python3 .agent/scratch/index-wp17/verify-post-format-packages.py
```

Two Cargo jobs and the 5 GiB MemoryHigh / 6 GiB MemoryMax scope produce 14
first-party packages, eight metapackages and selected local LiminalView. Actual
controls, sizes and hashes match `target/debian/build-set.json`; seven runtime
stamps contain its ID. Mixed-set and stale-build-script checks pass.

## Packaged executables

Owned extraction with disposable HOME/runtime, no display and explicit nonexistent
bus addresses: the compositor's `--check` and preview helper's `--self-check` both
exit zero. The PDF renderer/license, filed unit and Index1 XML are present and
hashed. The compositor check exercises protocol/socket state without DRM or HDMI.

## Source authority

Archive: `target/source-archives/templaros-workspace-b7b5d0197ce38add31116d3305e17eb4b91bd90745250914adf1f675d445ff07.tar.gz`.
SHA256: `b7b5d0197ce38add31116d3305e17eb4b91bd90745250914adf1f675d445ff07`.
Closed content digest: `7d0c5ec33eaf28e4b588aedd348419ba25dded9542570e438fab299392d8cb4c`.
All **625 archived first-party Rust/TOML/lock files** match the native
working tree at verification. Other assets are not compared.

First-party packages use the shared native tree. The retained archive was not
used for an isolated workspace rebuild. LiminalView was rebuilt offline from
its closed archive `76853c573b65086b8facec73bc1a64446be7016a488cf0a2d8397efe991940de`;
its sidecar retains source/toolchain provenance. No public release origin or pin
is claimed. The prior post-lint and original package cohorts remain separate in
the JSON; their counts and artifacts are not combined.

## Remaining qualification

Nothing was installed. The installed developer day, real NTFS unplug/return and
physical HDMI journey remain owner-run. FSearch and Silvaview prototypes are not
selected packages. The owner deferred FSearch adoption for the future own Index1
GUI. The [latest native gates](index-formatting-2026-10-09-wp17.md) pass required
lint, 1,803 selected-package tests and full workspace formatting/compilation;
this is not a whole-workspace CI claim. The WP8 oracle is retained.
