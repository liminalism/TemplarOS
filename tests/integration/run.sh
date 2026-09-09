#!/usr/bin/env bash
# Smoke: daemons start in dry-run mode, templar-hostd/templar-boot-health run their read-only probes
# against a fixture root, templar-shell lists/prints its drawers, and the CLI's doctor and bus-backed
# subcommands are exercised against a fixture image root with no session bus service behind them.
#
# The build is a separate, earlier step on purpose. This script asserts things about *runtime*
# behaviour, and a compile error is not a data point about runtime behaviour — it is a hard stop.
# Building first and then invoking the produced binaries directly (never `cargo run`) keeps the two
# failures from being conflated: after the gate below, a non-zero exit from any command here can
# only mean the behaviour being asserted, never a broken build.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"

# Run the smoke in a private home and a private session bus. Without this wrapper, app discovery
# and MIME import silently consume the developer's real ~/.config and ~/.local/share, so a green
# result depends on whatever happens to be installed on that workstation.
if [ "${TEMPLAR_INTEGRATION_ISOLATED:-0}" != 1 ]; then
    if ! command -v dbus-run-session >/dev/null 2>&1; then
        echo "integration smoke: FAILED — dbus-run-session is required for isolation" >&2
        exit 1
    fi
    isolated_root="$(mktemp -d /tmp/templaros-integration.XXXXXX)"
    integration_cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    integration_rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
    cleanup() {
        case "$isolated_root" in
            /tmp/templaros-integration.*) rm -rf -- "$isolated_root" ;;
            *) echo "refusing to remove unexpected integration path: $isolated_root" >&2 ;;
        esac
    }
    trap cleanup EXIT
    mkdir -p "$isolated_root"/{home,config,data,state,cache,runtime}
    chmod 700 "$isolated_root/runtime"
    dbus-run-session -- env \
        TEMPLAR_INTEGRATION_ISOLATED=1 \
        TEMPLAR_INTEGRATION_ROOT="$isolated_root" \
        CARGO_HOME="$integration_cargo_home" \
        RUSTUP_HOME="$integration_rustup_home" \
        HOME="$isolated_root/home" \
        XDG_CONFIG_HOME="$isolated_root/config" \
        XDG_DATA_HOME="$isolated_root/data" \
        XDG_DATA_DIRS="$isolated_root/data" \
        XDG_STATE_HOME="$isolated_root/state" \
        XDG_CACHE_HOME="$isolated_root/cache" \
        XDG_RUNTIME_DIR="$isolated_root/runtime" \
        bash "$root/tests/integration/run.sh"
    exit $?
fi
: "${TEMPLAR_INTEGRATION_ROOT:?isolated wrapper did not provide TEMPLAR_INTEGRATION_ROOT}"
cd "$root"

bin="${CARGO_TARGET_DIR:-$root/target}/debug"

if ! cargo build --workspace; then
    echo >&2
    echo "integration smoke: FAILED — the workspace does not compile." >&2
    echo "Nothing below this point is meaningful until that is fixed; refusing to report ok." >&2
    exit 1
fi

# Runs an already-built command that needs a D-Bus service this script never starts, and asserts
# it degrades cleanly. `service` is the org.templaros.* name it must be unable to reach.
#
# Three separate assertions, because only all three together mean "handled the absence gracefully":
#   1. a non-zero exit          — exiting 0 would mean it served stale or fabricated data;
#   2. no Rust panic/backtrace  — it must report the condition, not crash on it;
#   3. the output names the service it could not reach, or says the bus itself was unreachable —
#      without this, a segfault, a usage error or a missing-file error would all count as evidence
#      of graceful bus-absence handling, which is exactly the false green this check exists to
#      prevent. Both accepted shapes are real and observed: with a session bus present but the
#      service absent, zbus reports `ServiceUnknown: The name org.templaros.X was not provided ...`;
#      with no reachable bus at all, `templar` reports its own "connecting to the ... bus" context.
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

session_summary="$("$bin/templar-sessiond" --dry-run --manifest-dir tests/fixtures/manifests \
    --application-dir "$TEMPLAR_INTEGRATION_ROOT/data/applications" \
    --no-default-discovery \
    --state-db "$TEMPLAR_INTEGRATION_ROOT/state/session.db")"
if ! grep -q 'manifests:    1 native, 0 imported from .desktop files' <<<"$session_summary"; then
    echo "integration smoke: FAILED — sessiond discovery escaped the isolated fixture" >&2
    echo "$session_summary" >&2
    exit 1
fi
echo "templar-sessiond discovery: isolated (1 fixture manifest, 0 host desktop files)"
"$bin/templar-resourced" --once
"$bin/templar-filed" --dry-run >/dev/null
"$bin/templar-session" --dry-run
# hostd has no --dry-run of its own; --probe is its read-only equivalent — it prints the detected
# hardware profile and returns before claiming org.templaros.Host on the system bus.
"$bin/templar-hostd" --probe --root tests/fixtures/image-root >/dev/null
# --report-only never arms the reboot path, but the report still has to tell the truth. This
# fixture intentionally has neither runtime marker, so both wired checks must fail and the overall
# report must be unhealthy. Merely accepting exit 0 here used to turn UNHEALTHY into a false green.
boot_report="$TEMPLAR_INTEGRATION_ROOT/boot-health.json"
"$bin/templar-boot-health" --report-only --root tests/fixtures/image-root >"$boot_report"
grep -q '"healthy": false' "$boot_report"
grep -q '"session_healthy_report_wired": true' "$boot_report"
grep -q '"compositor_active_report_wired": true' "$boot_report"
for marker in compositor-active session-healthy; do
    if ! grep -A2 "\"name\": \"$marker\"" "$boot_report" | grep -q '"status": "fail"'; then
        echo "integration smoke: FAILED — $marker was not reported as a failed wired check" >&2
        cat "$boot_report" >&2
        exit 1
    fi
done
echo "templar-boot-health: expected unhealthy fixture confirmed"

# The compositor, headlessly: constructs every protocol global, binds both its public and
# trusted-shell sockets, and exits. This is the one compositor mode that runs where there is no
# seat and no host compositor to nest inside, which is everywhere except a developer's own
# session. Rendering and input are not covered here and are not claimed to be — those are the
# nested and on-hardware checks. `src/backend/check.rs`'s own test then connects a real Wayland
# client over the socket and asserts the advertised protocol set, including that an ordinary
# client cannot see the trusted-shell protocols.
"$bin/templar-comp" --check >/dev/null
echo "templar-comp --check: ok"

# `--exec` is how the greeter is started, so the compositor must exit when its client does and
# report that client's exit code. Both directions are checked, and the immediate-exit case is the
# regression test: calloop's EventLoop::run clears the stop flag on entry, so a client that exits
# before the loop starts once left the compositor blocked forever — a greeter that dies on startup
# would have meant a black screen with no way to log in. /bin/true is as immediate as it gets.
"$bin/templar-comp" --check --exec /bin/true >/dev/null 2>&1
exec_status=0
"$bin/templar-comp" --check --exec /bin/false >/dev/null 2>&1 || exec_status=$?
if [ "$exec_status" -ne 1 ]; then
    echo "templar-comp --exec did not propagate its client's exit code (got $exec_status, want 1)" >&2
    exit 1
fi
echo "templar-comp --exec: ok (exits with its client, propagates its code)"

"$bin/templar-shell" --list-drawers >/dev/null
# `--print <drawer>` renders every Home Surface drawer as text with no session bus, no live
# compositor and no toolkit — the direct fulfilment of "everything the shell shows must be
# available through templar too" for the shell's own text form.
"$bin/templar-shell" --print recent >/dev/null
echo "templar-shell --print: ok"

"$bin/templar" doctor --root tests/fixtures/image-root --json >/dev/null || [ $? -ne 0 ]
"$bin/templar" doctor --root tests/fixtures/image-root >/dev/null || [ $? -ne 0 ]

# Every bus-backed `templar` subcommand must degrade the same way, naming what it could not reach.
# None of these services run on this script's bus, by design.
expect_clean_failure org.templaros.Session "$bin/templar" app list
expect_clean_failure org.templaros.Session "$bin/templar" open --explain "$root/tests/fixtures/image-root/etc/os-release"
expect_clean_failure org.templaros.Resources "$bin/templar" res
expect_clean_failure org.templaros.Files "$bin/templar" files volumes
expect_clean_failure org.templaros.Session "$bin/templar" history
expect_clean_failure org.templaros.Session "$bin/templar" providers
expect_clean_failure org.templaros.Session "$bin/templar" settings
expect_clean_failure org.templaros.Session "$bin/templar" project
expect_clean_failure org.templaros.Session "$bin/templar" power
expect_clean_failure org.templaros.Host "$bin/templar" update

echo "integration smoke: ok"
