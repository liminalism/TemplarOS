#!/usr/bin/env bash
# Smoke: daemons start in dry-run mode, lege-hostd/lege-boot-health run their read-only probes
# against a fixture root, lege-shell lists/prints its drawers, and the CLI's doctor and bus-backed
# subcommands are exercised against a fixture image root with no session bus service behind them.
#
# The build is a separate, earlier step on purpose. This script asserts things about *runtime*
# behaviour, and a compile error is not a data point about runtime behaviour — it is a hard stop.
# Building first and then invoking the produced binaries directly (never `cargo run`) keeps the two
# failures from being conflated: after the gate below, a non-zero exit from any command here can
# only mean the behaviour being asserted, never a broken build.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

bin="${CARGO_TARGET_DIR:-$root/target}/debug"

if ! cargo build --workspace; then
    echo >&2
    echo "integration smoke: FAILED — the workspace does not compile." >&2
    echo "Nothing below this point is meaningful until that is fixed; refusing to report ok." >&2
    exit 1
fi

# Runs an already-built command that needs a D-Bus service this script never starts, and asserts
# it degrades cleanly. `service` is the org.legeos.* name it must be unable to reach.
#
# Three separate assertions, because only all three together mean "handled the absence gracefully":
#   1. a non-zero exit          — exiting 0 would mean it served stale or fabricated data;
#   2. no Rust panic/backtrace  — it must report the condition, not crash on it;
#   3. the output names the service it could not reach, or says the bus itself was unreachable —
#      without this, a segfault, a usage error or a missing-file error would all count as evidence
#      of graceful bus-absence handling, which is exactly the false green this check exists to
#      prevent. Both accepted shapes are real and observed: with a session bus present but the
#      service absent, zbus reports `ServiceUnknown: The name org.legeos.X was not provided ...`;
#      with no reachable bus at all, `lege` reports its own "connecting to the ... bus" context.
expect_clean_failure() {
    local service="$1"
    shift
    local out status=0
    out="$("$@" 2>&1)" || status=$?
    if [ "$status" -eq 0 ]; then
        echo "expected '$*' to fail without $service on the bus, but it exited 0:" >&2
        echo "$out" >&2
        exit 1
    fi
    if grep -q "panicked at" <<<"$out"; then
        echo "'$*' panicked instead of failing cleanly:" >&2
        echo "$out" >&2
        exit 1
    fi
    if ! grep -qE "$service|connecting to the (session|system) bus" <<<"$out"; then
        echo "'$*' failed, but not with a message naming $service or an unreachable bus;" >&2
        echo "that is a different failure than the one this check asserts:" >&2
        echo "$out" >&2
        exit 1
    fi
}

"$bin/lege-sessiond" --dry-run --manifest-dir tests/fixtures/manifests --state-db "$(mktemp -d)/s.db"
"$bin/lege-resourced" --once
"$bin/lege-filed" --dry-run >/dev/null
"$bin/lege-session" --dry-run
# hostd has no --dry-run of its own; --probe is its read-only equivalent — it prints the detected
# hardware profile and returns before claiming org.legeos.Host on the system bus.
"$bin/lege-hostd" --probe --root tests/fixtures/image-root >/dev/null
# --report-only is boot-health's dry-run: it always exits 0 and only prints the report, never
# arming the reboot path (that path is covered by services/lege-boot-health/src/main.rs's own
# unit tests, not this smoke script).
"$bin/lege-boot-health" --report-only --root tests/fixtures/image-root >/dev/null

"$bin/lege-shell" --list-drawers >/dev/null
# `--print <drawer>` renders every Home Surface drawer as text with no session bus, no live
# compositor and no toolkit — the direct fulfilment of "everything the shell shows must be
# available through lege too" for the shell's own text form.
"$bin/lege-shell" --print recent >/dev/null
echo "lege-shell --print: ok"

"$bin/lege" doctor --root tests/fixtures/image-root --json >/dev/null || [ $? -ne 0 ]
"$bin/lege" doctor --root tests/fixtures/image-root >/dev/null || [ $? -ne 0 ]

# Every bus-backed `lege` subcommand must degrade the same way, naming what it could not reach.
# None of these services run on this script's bus, by design.
expect_clean_failure org.legeos.Session "$bin/lege" app list
expect_clean_failure org.legeos.Session "$bin/lege" open --explain "$root/tests/fixtures/image-root/etc/os-release"
expect_clean_failure org.legeos.Resources "$bin/lege" res
expect_clean_failure org.legeos.Files "$bin/lege" files volumes
expect_clean_failure org.legeos.Session "$bin/lege" history
expect_clean_failure org.legeos.Session "$bin/lege" providers
expect_clean_failure org.legeos.Session "$bin/lege" settings
expect_clean_failure org.legeos.Session "$bin/lege" project
expect_clean_failure org.legeos.Session "$bin/lege" power
expect_clean_failure org.legeos.Host "$bin/lege" update

echo "integration smoke: ok"
