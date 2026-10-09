#!/usr/bin/env bash
# Owner-run only. Build the companion as your user before running this script with sudo.
# cargo build -p templar-filed --example index_volume_fixture
# sudo scripts/index-volume-fixture.sh --binary /absolute/path/to/index_volume_fixture
# Pipe stdout/stderr to a log and retain it as AKR evidence after a successful run.
set -euo pipefail
binary=
dry_run=0
while (($#)); do
  case $1 in
    --binary) binary=${2:?missing companion binary}; shift 2 ;;
    --dry-run) dry_run=1; shift ;;
    -h|--help)
      printf '%s\n' 'usage: sudo index-volume-fixture.sh --binary PATH [--dry-run]' \
        'Creates two private 64 MiB ext4 loopback images under /tmp and runs:' \
        'mount/index -> detach -> relocate -> return/watch rearm -> mount-over/unmount -> replacement/accept.' \
        'No session bus, installed services, existing mounts or existing disk contents are used.'
      exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done
if ((dry_run)); then
  printf '%s\n' 'Create owned /tmp/templar-volume-fixture.XXXXXX; format two fresh regular image files.' \
    'Attach only those images to loops; expose their verified UUID symlinks if udev has not yet done so.' \
    'Mount original image; run companion as invoking user with a private database and watcher.' \
    'Assert detach retention, relocation without following, return/rearm, mount-over boundary repair, fenced replacement.' \
    'Join companion; unmount only recorded fixture loops; remove owned links, detach loops and remove private directory.'
  exit 0
fi
[[ $(id -u) == 0 && -n ${SUDO_USER:-} && $SUDO_USER != root ]] || {
  printf '%s\n' 'Run with sudo from your native user account; cargo must be built beforehand.' >&2; exit 2;
}
[[ $binary == /* && -x $binary && -f $binary ]] || { printf '%s\n' '--binary must name the already built absolute companion executable.' >&2; exit 2; }
for tool in mkfs.ext4 losetup blkid mount umount findmnt runuser; do command -v "$tool" >/dev/null; done
base=$(mktemp -d /tmp/templar-volume-fixture.XXXXXX)
loops=()
links=()
fixture_pid=
cleanup() {
  local point dev link source
  if [[ -n $fixture_pid ]] && kill -0 "$fixture_pid" 2>/dev/null; then
    kill "$fixture_pid" 2>/dev/null || true
    wait "$fixture_pid" 2>/dev/null || true
  fi
  for point in "$base/original/notes" "$base/relocated" "$base/original"; do
    source=$(findmnt -rn -M "$point" -o SOURCE 2>/dev/null || true)
    [[ -n $source ]] || continue
    for dev in "${loops[@]}"; do
      if [[ $source == "$dev" ]]; then umount "$point" || return; fi
    done
    if findmnt -rn -M "$point" >/dev/null 2>&1; then
      printf 'Refusing cleanup of unexpected mount at %s\n' "$point" >&2; return
    fi
  done
  for link in "${links[@]}"; do
    for dev in "${loops[@]}"; do
      if [[ -L $link && $(readlink -f -- "$link") == "$dev" ]]; then rm -- "$link"; fi
    done
  done
  for dev in "${loops[@]}"; do
    # Detach only a loop still backed by one of the files this run created.
    source=$(losetup -n -O BACK-FILE "$dev" 2>/dev/null || true)
    if [[ $source == "$base/a.img" || $source == "$base/b.img" ]]; then losetup -d "$dev"; fi
  done
  rm -rf -- "$base"
}
trap cleanup EXIT
mkdir "$base/original" "$base/relocated"
chown "$SUDO_USER" "$base"
chmod 755 "$base"
mkdir -p /dev/disk/by-uuid
for letter in a b; do
  image="$base/$letter.img"
  truncate -s 64M "$image"
  mkfs.ext4 -q -F "$image"
  dev=$(losetup --find --show "$image")
  loops+=("$dev")
  uuid=$(blkid -p -s UUID -o value "$dev")
  link="/dev/disk/by-uuid/$uuid"
  if [[ -e $link || -L $link ]]; then
    [[ $(readlink -f -- "$link") == "$dev" ]] || { printf 'UUID link collision: %s\n' "$link" >&2; exit 1; }
  else
    ln -s "$dev" "$link"
    links+=("$link")
  fi
done
mount "${loops[0]}" "$base/original"
mkdir "$base/original/notes"
printf one > "$base/original/notes/original.txt"
chown -R "$SUDO_USER" "$base/original"
coproc JOURNEY { runuser -u "$SUDO_USER" -- env TEMPLAR_VOLUME_FIXTURE="$base" "$binary"; }
fixture_pid=$JOURNEY_PID
exec {journey_in}>&"${JOURNEY[1]}"
exec {journey_out}<&"${JOURNEY[0]}"
expect() {
  local expected=$1 line
  if ! IFS= read -r -t 45 -u "$journey_out" line; then printf 'Companion failed or timed out at %s\n' "$expected" >&2; exit 1; fi
  printf '%s\n' "$line"
  [[ $line == "$expected" ]] || { printf 'Expected %s\n' "$expected" >&2; exit 1; }
}
stage() { printf '%s\n' "$1" >&"$journey_in"; expect "PASS $1"; }
expect 'READY initial'
umount "$base/original"
stage absent
mount "${loops[0]}" "$base/relocated"
stage relocated
umount "$base/relocated"
mount "${loops[0]}" "$base/original"
stage returned
mount "${loops[1]}" "$base/original/notes"
stage mount-over
umount "$base/original/notes"
stage mount-removed
umount "$base/original"
stage absent
mount "${loops[1]}" "$base/original"
printf replacement > "$base/original/replacement.txt"
chown -R "$SUDO_USER" "$base/original"
stage different
printf 'done\n' >&"$journey_in"
expect 'PASS volume journey; watcher and mount monitor joined'
wait "$fixture_pid"
fixture_pid=
printf '%s\n' 'PASS owner-run loopback mount fixture'
