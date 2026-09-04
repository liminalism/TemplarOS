#!/usr/bin/env bash
# A loopback Btrfs fixture for the file-index delta spike (Lane E).
#
# This host is ext4 end-to-end, so the only way to answer "is a Btrfs metadata delta cheaper than
# a recursive reconciliation?" is to build a Btrfs filesystem in a file and measure inside it.
# Creating a loop device and mounting it needs root; everything after that does not, so this
# script does the privileged part once and hands the result to the invoking user.
#
#   sudo scripts/btrfs-index-fixture.sh setup
#   sudo scripts/btrfs-index-fixture.sh probe      # what works unprivileged, and what does not
#   sudo scripts/btrfs-index-fixture.sh measure    # build as you, run the harness as root
#   sudo scripts/btrfs-index-fixture.sh teardown
#
# Layout mirrors distro/deploy/layout.toml: @, @home, @var, @snapshots, @deployments.
#
# Two deliberate choices about *where*:
#   * The backing image lives on a filesystem with room (/mnt/Samsung980_1TB has ~80 GB free;
#     / has ~8 GB, so it is never the default).
#   * The mount point lives under /tmp, because crates/lege-file-index/src/watch.rs's fixture
#     guard panics when a test binary watches anything outside std::env::temp_dir(). Mounting the
#     fixture inside /tmp is what makes the harness runnable as an ordinary integration test
#     rather than a bespoke binary; the bytes still come from the image on the big disk.
#
# Safety rules this script holds to:
#   * It only ever formats a *regular file* it created or was pointed at, never a block device.
#   * It refuses any image path under /dev, and refuses a path that exists and is not a file.
#   * It detaches only the loop device recorded in its own state file.
#   * It prints the exact commands it is about to run, and --dry-run stops before running them.

set -euo pipefail

IMAGE=${LEGE_BTRFS_IMAGE:-/mnt/Samsung980_1TB/legeos-btrfs-fixture.img}
MOUNT=${LEGE_BTRFS_FIXTURE:-/tmp/legeos-btrfs-fixture}
SIZE=${LEGE_BTRFS_SIZE:-24G}
DRY_RUN=0
ASSUME_YES=0
COMMAND=""

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
SUBVOLUMES=("@" "@home" "@var" "@snapshots" "@deployments")

usage() {
  cat <<'EOF'
usage: btrfs-index-fixture.sh [options] <setup|probe|status|measure|teardown>

commands
  setup      create the image, attach a loop device, mkfs.btrfs, mount, create the subvolumes,
             and hand @home/@snapshots to the invoking user. Idempotent.
  probe      report which btrfs operations the fixture allows unprivileged (snapshot, seal
             read-only, send, delete). This is the fact that decides whether the delta backend
             can live in the per-user daemon or must go through lege-hostd.
  status     show what exists right now.
  measure    build the Lane E harness as the invoking user, then run it against the fixture.
  teardown   unmount, detach the loop device, and (with --remove-image) delete the image.

options
  --image PATH   backing file           (default: /mnt/Samsung980_1TB/legeos-btrfs-fixture.img)
  --mount PATH   mount point            (default: /tmp/legeos-btrfs-fixture)
  --size N       image size for mkfs    (default: 24G)
  --remove-image delete the image during teardown
  --dry-run      print the plan and stop
  --yes          do not ask for confirmation
  -h, --help     this text
EOF
}

REMOVE_IMAGE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --image) IMAGE=$2; shift 2 ;;
    --mount) MOUNT=$2; shift 2 ;;
    --size) SIZE=$2; shift 2 ;;
    --remove-image) REMOVE_IMAGE=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --yes) ASSUME_YES=1; shift ;;
    -h|--help) usage; exit 0 ;;
    setup|probe|status|measure|teardown) COMMAND=$1; shift ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z $COMMAND ]]; then
  usage >&2
  exit 2
fi

STATE="${IMAGE}.loop"

# The user who invoked sudo — everything inside the fixture is theirs, not root's.
OWNER=${SUDO_USER:-$(id -un)}
OWNER_UID=$(id -u "$OWNER")
OWNER_GID=$(id -g "$OWNER")

say() { printf '%s\n' "$*"; }
step() { printf '  %s\n' "$*"; }

run() {
  printf '  $ %s\n' "$*"
  if [[ $DRY_RUN -eq 0 ]]; then
    "$@"
  fi
}

as_owner() {
  printf '  $ (as %s) %s\n' "$OWNER" "$*"
  if [[ $DRY_RUN -eq 0 ]]; then
    sudo -u "$OWNER" "$@"
  fi
}

require_root() {
  # --dry-run only prints; a reviewer must be able to read the plan without handing over root.
  if [[ $DRY_RUN -eq 1 ]]; then
    return 0
  fi
  if [[ $(id -u) -ne 0 ]]; then
    echo "error: '$COMMAND' needs root (loop devices and mount(2) are privileged)." >&2
    echo "       run: sudo $0 $COMMAND" >&2
    exit 1
  fi
}

confirm() {
  [[ $ASSUME_YES -eq 1 || $DRY_RUN -eq 1 ]] && return 0
  read -r -p "proceed? [y/N] " reply
  [[ $reply == y || $reply == Y ]]
}

# --- safety -----------------------------------------------------------------------------------

validate_image_path() {
  case "$IMAGE" in
    /dev/*)
      echo "error: refusing to touch $IMAGE — this script only ever formats a regular file." >&2
      exit 1
      ;;
    /*) ;;
    *)
      echo "error: --image must be an absolute path (got $IMAGE)." >&2
      exit 1
      ;;
  esac
  if [[ -e $IMAGE && ! -f $IMAGE ]]; then
    echo "error: $IMAGE exists and is not a regular file. Refusing." >&2
    exit 1
  fi
  local parent
  parent=$(dirname -- "$IMAGE")
  if [[ ! -d $parent ]]; then
    echo "error: $parent does not exist; create it or pass a different --image." >&2
    exit 1
  fi
}

validate_mount_path() {
  case "$MOUNT" in
    /) echo "error: refusing / as a mount point." >&2; exit 1 ;;
    /*) ;;
    *) echo "error: --mount must be an absolute path (got $MOUNT)." >&2; exit 1 ;;
  esac
  local tmp=${TMPDIR:-/tmp}
  if [[ $MOUNT != "$tmp"/* ]]; then
    say "warning: the mount point $MOUNT is not under $tmp."
    say "         crates/lege-file-index/src/watch.rs's fixture guard panics on a watch root"
    say "         outside the temporary directory, so the integration harness will refuse to run."
  fi
}

loop_device() {
  [[ -f $STATE ]] && cat "$STATE"
}

# --- commands ---------------------------------------------------------------------------------

cmd_status() {
  say "image:        $IMAGE"
  if [[ -f $IMAGE ]]; then
    step "exists, apparent size $(du -h --apparent-size "$IMAGE" | cut -f1), on disk $(du -h "$IMAGE" | cut -f1)"
  else
    step "absent"
  fi
  local dev
  dev=$(loop_device || true)
  say "loop device:  ${dev:-none recorded}"
  if [[ -n ${dev:-} ]] && losetup "$dev" >/dev/null 2>&1; then
    step "attached: $(losetup --noheadings --output BACK-FILE "$dev")"
  fi
  say "mount point:  $MOUNT"
  if mountpoint -q "$MOUNT" 2>/dev/null; then
    step "mounted"
    if [[ -d $MOUNT ]]; then
      btrfs subvolume list "$MOUNT" 2>/dev/null | sed 's/^/    /' || true
    fi
  else
    step "not mounted"
  fi
}

cmd_setup() {
  require_root
  validate_image_path
  validate_mount_path

  say "Plan:"
  step "create $IMAGE as a $SIZE sparse regular file (if absent)"
  step "attach it to a free loop device and record it in $STATE"
  step "mkfs.btrfs the loop device (only if it is not already btrfs)"
  step "mount it at $MOUNT with compress=zstd:1,user_subvol_rm_allowed"
  step "create subvolumes: ${SUBVOLUMES[*]}"
  step "chown $MOUNT and its subvolumes to $OWNER ($OWNER_UID:$OWNER_GID)"
  confirm || { say "aborted."; exit 0; }

  if [[ ! -f $IMAGE ]]; then
    run truncate -s "$SIZE" "$IMAGE"
  else
    step "$IMAGE already exists; leaving it alone"
  fi

  local dev
  dev=$(loop_device || true)
  if [[ -n ${dev:-} ]] && losetup "$dev" >/dev/null 2>&1; then
    step "loop device $dev already attached"
  else
    if [[ $DRY_RUN -eq 0 ]]; then
      dev=$(losetup --find --show "$IMAGE")
      printf '%s\n' "$dev" >"$STATE"
      step "attached $dev"
    else
      printf '  $ %s\n' "losetup --find --show $IMAGE"
      dev="/dev/loopN"
    fi
  fi

  if [[ $DRY_RUN -eq 0 ]] && blkid -o value -s TYPE "$dev" 2>/dev/null | grep -qx btrfs; then
    step "$dev already carries a btrfs filesystem; not reformatting"
  else
    run mkfs.btrfs -L legeos-fixture -f "$dev"
  fi

  run mkdir -p "$MOUNT"
  if mountpoint -q "$MOUNT" 2>/dev/null; then
    step "$MOUNT already mounted"
  else
    run mount -o compress=zstd:1,user_subvol_rm_allowed "$dev" "$MOUNT"
  fi

  for subvolume in "${SUBVOLUMES[@]}"; do
    if [[ -d "$MOUNT/$subvolume" ]]; then
      step "$MOUNT/$subvolume already exists"
    else
      run btrfs subvolume create "$MOUNT/$subvolume"
    fi
  done

  run chown "$OWNER_UID:$OWNER_GID" "$MOUNT"
  for subvolume in "${SUBVOLUMES[@]}"; do
    run chown "$OWNER_UID:$OWNER_GID" "$MOUNT/$subvolume"
  done
  run mkdir -p "$MOUNT/index-db" "$MOUNT/streams"
  run chown "$OWNER_UID:$OWNER_GID" "$MOUNT/index-db" "$MOUNT/streams"

  say ""
  say "Fixture ready. The harness reads LEGE_BTRFS_FIXTURE:"
  say "  export LEGE_BTRFS_FIXTURE=$MOUNT"
  say "Then, from $REPO_ROOT:"
  say "  cargo test -p lege-file-index --test btrfs_delta_fixture -- --ignored --nocapture"
  say "(run 'sudo $0 probe' first — btrfs send may still require CAP_SYS_ADMIN)"
}

cmd_probe() {
  require_root
  if ! mountpoint -q "$MOUNT" 2>/dev/null; then
    echo "error: $MOUNT is not mounted. Run 'sudo $0 setup' first." >&2
    exit 1
  fi

  say "Probing what the fixture allows as $OWNER (uid $OWNER_UID) versus as root."
  say ""

  local home="$MOUNT/@home" snaps="$MOUNT/@snapshots"
  rm -rf "$snaps/probe-a" "$snaps/probe-b" 2>/dev/null || true
  sudo -u "$OWNER" mkdir -p "$home/probe" 2>/dev/null || true
  sudo -u "$OWNER" sh -c "printf one >'$home/probe/one'" 2>/dev/null || true

  local unpriv_snapshot=no unpriv_seal=no unpriv_send=no unpriv_delete=no

  if sudo -u "$OWNER" btrfs subvolume snapshot -r "$home" "$snaps/probe-a" >/dev/null 2>&1; then
    unpriv_snapshot=yes
  else
    btrfs subvolume snapshot -r "$home" "$snaps/probe-a" >/dev/null
  fi
  sudo -u "$OWNER" sh -c "printf two >'$home/probe/two'" 2>/dev/null || true
  if sudo -u "$OWNER" btrfs subvolume snapshot -r "$home" "$snaps/probe-b" >/dev/null 2>&1; then
    unpriv_seal=yes
  else
    btrfs subvolume snapshot -r "$home" "$snaps/probe-b" >/dev/null
  fi

  if sudo -u "$OWNER" btrfs send --no-data -p "$snaps/probe-a" -f /dev/null "$snaps/probe-b" \
      >/dev/null 2>&1; then
    unpriv_send=yes
  fi
  say "root-side send, for comparison:"
  if btrfs send --no-data -p "$snaps/probe-a" -f "$MOUNT/streams/probe.stream" "$snaps/probe-b" \
      >/dev/null 2>&1; then
    step "ok — $(stat -c %s "$MOUNT/streams/probe.stream") bytes of metadata delta"
  else
    step "FAILED even as root; the fixture is not usable for this lane"
  fi

  if sudo -u "$OWNER" btrfs subvolume delete "$snaps/probe-b" >/dev/null 2>&1; then
    unpriv_delete=yes
  else
    btrfs subvolume delete "$snaps/probe-b" >/dev/null 2>&1 || true
  fi
  btrfs subvolume delete "$snaps/probe-a" >/dev/null 2>&1 || true
  rm -rf "$home/probe"

  say ""
  say "unprivileged results (as $OWNER):"
  step "btrfs subvolume snapshot -r : $unpriv_snapshot"
  step "second read-only snapshot   : $unpriv_seal"
  step "btrfs send --no-data -p     : $unpriv_send"
  step "btrfs subvolume delete      : $unpriv_delete"
  say ""
  if [[ $unpriv_send == yes ]]; then
    say "=> send works unprivileged here; the harness can run as $OWNER:"
    say "   LEGE_BTRFS_FIXTURE=$MOUNT cargo test -p lege-file-index \\"
    say "       --test btrfs_delta_fixture -- --ignored --nocapture"
  else
    say "=> send needs CAP_SYS_ADMIN. Use 'sudo $0 measure', which builds the harness as"
    say "   $OWNER and then runs only the already-built test binary as root."
  fi
}

cmd_measure() {
  require_root
  if ! mountpoint -q "$MOUNT" 2>/dev/null; then
    echo "error: $MOUNT is not mounted. Run 'sudo $0 setup' first." >&2
    exit 1
  fi

  say "Plan:"
  step "build the harness as $OWNER (cargo never runs as root, so target/ stays yours)"
  step "run the built test binary as root, with LEGE_BTRFS_FIXTURE=$MOUNT"
  step "write the report to $REPO_ROOT/docs/measurements/"
  confirm || { say "aborted."; exit 0; }

  local json binary
  json=$(sudo -u "$OWNER" env -C "$REPO_ROOT" \
    cargo test -p lege-file-index --test btrfs_delta_fixture --no-run --message-format=json)
  binary=$(printf '%s' "$json" | python3 -c '
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable"):
        target = message.get("target", {})
        if target.get("name") == "btrfs_delta_fixture":
            print(message["executable"])
')
  if [[ -z $binary ]]; then
    echo "error: could not find the built btrfs_delta_fixture binary." >&2
    exit 1
  fi
  step "harness: $binary"

  LEGE_BTRFS_FIXTURE="$MOUNT" \
  LEGE_BTRFS_REPORT_DIR="$REPO_ROOT/docs/measurements" \
  LEGE_BTRFS_REPORT_OWNER="$OWNER_UID:$OWNER_GID" \
    "$binary" --ignored --nocapture --test-threads=1

  say ""
  say "Done. Any report written under docs/measurements/ is owned by $OWNER."
}

cmd_teardown() {
  require_root
  say "Plan:"
  step "unmount $MOUNT (if mounted)"
  step "detach the loop device recorded in $STATE"
  if [[ $REMOVE_IMAGE -eq 1 ]]; then
    step "delete $IMAGE"
  else
    step "keep $IMAGE (pass --remove-image to delete it)"
  fi
  confirm || { say "aborted."; exit 0; }

  if mountpoint -q "$MOUNT" 2>/dev/null; then
    run umount "$MOUNT"
  else
    step "$MOUNT was not mounted"
  fi

  local dev
  dev=$(loop_device || true)
  if [[ -n ${dev:-} ]]; then
    if losetup "$dev" >/dev/null 2>&1; then
      local backing
      backing=$(losetup --noheadings --output BACK-FILE "$dev" | tr -d ' ')
      if [[ $backing == "$IMAGE" ]]; then
        run losetup -d "$dev"
      else
        say "warning: $dev now backs $backing, not $IMAGE. Not detaching it."
      fi
    fi
    run rm -f "$STATE"
  else
    step "no loop device recorded"
  fi

  if [[ $REMOVE_IMAGE -eq 1 && -f $IMAGE ]]; then
    run rm -f "$IMAGE"
  fi
  run rmdir "$MOUNT" 2>/dev/null || true
}

case "$COMMAND" in
  setup) cmd_setup ;;
  probe) cmd_probe ;;
  status) cmd_status ;;
  measure) cmd_measure ;;
  teardown) cmd_teardown ;;
esac
