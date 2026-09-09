#!/usr/bin/env bash
# The one privileged step of the fanotify spike (Lane D).
#
# Everything about *unprivileged* fanotify is measurable without this script and already is —
# see docs/measurements/fanotify-2026-09-04.md. Two questions are not, because the kernel refuses
# `FAN_MARK_MOUNT` and `FAN_MARK_FILESYSTEM` to anything without CAP_SYS_ADMIN:
#
#   1. Does a single `FAN_MARK_FILESYSTEM` mark actually work, and what does it cost?
#   2. Does it work on a **Btrfs subvolume**? Since Linux 6.8 a filesystem or mount mark on a
#      subvolume whose fsid differs from the superblock root's returns EXDEV. TemplarOS's declared
#      layout (distro/deploy/layout.toml) is @, @home, @var, @snapshots, @deployments — all
#      separate subvolumes — so if @home cannot be marked, the privileged variant is dead for
#      TemplarOS whatever its merits elsewhere.
#
# What this script does, and nothing else:
#   * builds the measurement harness AS THE INVOKING USER (never as root);
#   * runs it as root against the paths you name, plus the Btrfs loopback fixture if Lane E's
#     scripts/btrfs-index-fixture.sh has one mounted;
#   * prints the results.
#
# It creates no filesystem, mounts nothing, formats nothing and deletes nothing outside the
# temporary subdirectories the harness makes for itself under a path you named. It never touches
# a systemd unit or the live session.
#
#   sudo scripts/fanotify-privileged-probe.sh                # / , $HOME , the Btrfs fixture
#   sudo scripts/fanotify-privileged-probe.sh --dry-run      # print the plan, run nothing
#   sudo scripts/fanotify-privileged-probe.sh /some/mount    # a path of your choosing

set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BTRFS_FIXTURE=${TEMPLAR_BTRFS_FIXTURE:-/tmp/templaros-btrfs-fixture}
HARNESS="$REPO_ROOT/target/release/examples/fanotify-lab"
DRY_RUN=0
PATHS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,29p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) PATHS+=("$1"); shift ;;
  esac
done

OWNER=${SUDO_USER:-$(id -un)}

say() { printf '%s\n' "$*"; }
run() {
  printf '  $ %s\n' "$*"
  [[ $DRY_RUN -eq 1 ]] || "$@"
}
as_owner() {
  printf '  $ (as %s) %s\n' "$OWNER" "$*"
  [[ $DRY_RUN -eq 1 ]] || sudo -u "$OWNER" "$@"
}

if [[ $DRY_RUN -eq 0 && $(id -u) -ne 0 ]]; then
  say "error: this probe needs root — FAN_MARK_MOUNT and FAN_MARK_FILESYSTEM require CAP_SYS_ADMIN."
  say "       run: sudo $0"
  say "       (everything that does not need root is already measured in"
  say "        docs/measurements/fanotify-2026-09-04.md)"
  exit 1
fi

# Default paths: the root filesystem, the invoking user's home, and every subvolume of the Btrfs
# fixture if it happens to be mounted.
if [[ ${#PATHS[@]} -eq 0 ]]; then
  PATHS=(/ "$(eval echo "~$OWNER")")
  if mountpoint -q "$BTRFS_FIXTURE" 2>/dev/null; then
    say "found the Btrfs fixture at $BTRFS_FIXTURE (scripts/btrfs-index-fixture.sh)"
    for subvolume in "$BTRFS_FIXTURE" "$BTRFS_FIXTURE/@" "$BTRFS_FIXTURE/@home" "$BTRFS_FIXTURE/@snapshots"; do
      [[ -d $subvolume ]] && PATHS+=("$subvolume")
    done
  else
    say "no Btrfs fixture mounted at $BTRFS_FIXTURE."
    say "The EXDEV-on-subvolume question needs one. Create it first with:"
    say "  sudo scripts/btrfs-index-fixture.sh setup"
  fi
fi

say
say "building the harness as $OWNER (root never runs cargo here)"
as_owner cargo build --release -p templar-fanotify --examples

say
say "capability and mark matrix, as root"
run "$HARNESS" probe "${PATHS[@]}"

say
say "one filesystem mark per path, as root"
for path in "${PATHS[@]}"; do
  say "--- $path"
  run "$HARNESS" filesystem "$path" --dirs 200 || true
done

say
say "done. Fold the output into docs/measurements/fanotify-2026-09-04.md."
