# WP9 relocated volume discovery — 2026-10-08

Native source working tree over `12295f351ab14e649f900573190060cd88a7251e`.
The [JSON report](index-volumes-2026-10-08-relocation.json) records source hashes
and the focused volume test command: **13 passed, zero failed**.

At startup and mount-table events, the mount thread discovers a unique location
of a strongly identified volume. The native provider first resolves the recorded
UUID's block device and limits candidate mounts to that device. Each candidate
must retain the UUID and mount root, and its observed mount ID must match the
table. Weak identities, different subvolumes, shadowed candidates and ambiguous
locations supply no hint. There is no idle discovery timer or recursive scan.

`Roots` exposes an optional `mounted_elsewhere` native base64/display path. The
hint is fenced to the original recorded identity and appears only for offline
roots. It clears on failed observations, disappearance, return at the old path
or identity change. Root path, cached rows, policy and generation remain intact.

Settings shows the observed location and offers an explicit registration action.
It uses native bytes, registers the new path with the same policy, then removes
the old registration with its policy fence. The new registration needs a full
scan. Discovery never rewrites a path or accepts a replacement disk. The existing
manual absolute-path entry remains available when there is no reliable hint.

For a discovered location, Settings rechecks that the original registration is
still offline with the same strong identity. After registering the new path it
compares the new registration's UUID and mount root before removing the old root.
A changed or weak identity retains both registrations and the previous cached
contents. The 75-test Settings run passes, including this confirmation comparison.

The tests include non-UTF8 subdirectories, mountpoint roots, cloned/bind ambiguity,
UUID/root/mount-ID mismatches, observation failure, cache invalidation and retained
offline rows. The first run caught an empty-relative-path join adding a trailing
separator; preserving mountpoint bytes directly fixed that case.

These are injected-provider software observations. The owner-run loopback
[fixture](../../scripts/index-volume-fixture.sh) now also asserts the discovered
location, but its real mount journey has not been executed here. Physical NTFS
unplug/return and installed Settings remain unqualified.
