# Silvaview shared-index binary

WP15 is implemented in the owning sibling `../Silvaview-rs`, at
`413ea0422b37aaf849d501c60ff2d1350d67aab6` (implementation `aeeae4a5795b11dc1a69564245a46c8a274c3cd5`).
The AGPL-3.0-only app's optional Linux `templaros` feature links the MPL-2.0
store-free index reader. Its README records the minimal Index1 signatures and
source obligations. The local Vello fork remains a source dependency.

Build with the native target directory and the feature enabled:

```sh
CARGO_TARGET_DIR=$(scripts/native-target-dir.sh) CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 \
  cargo build --manifest-path ../Silvaview-rs/Cargo.toml --features templaros \
  --bin Silvaview-rs
```

This entry documents the qualified binary; it is not a Debian packaging overlay
and is not in the distro seed. Any later package must enable `templaros`, retain
the local Vello sources and both licenses, and qualify its actual executable.
See [the native/private/nested observations](../../../docs/measurements/index-silvaview-2026-10-08-wp15.md).

The subsequent owning commit `dbdeb29599e6026ae421c7ea6db29265eb50733c`
guards known-offline file-manager actions and preserves native clipboard paths.
Its 48 feature-on / 32 feature-off tests and refreshed private worker pass;
the actual nested GUI observations above retain their historical executable.
See [the action continuation](../../../docs/measurements/index-silvaview-2026-10-08-offline-actions.md).
