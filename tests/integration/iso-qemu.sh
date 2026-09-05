#!/usr/bin/env bash
# Boot the installation medium under QEMU with Secure Boot enforcing, and read the result off the
# serial console. Three honest outcomes: NOT EXECUTED (no ISO, no QEMU, no Secure-Boot OVMF),
# FAILED (the chain did not reach userspace), OK (the live system booted with Secure Boot on).
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
fail() { echo "iso-qemu: FAILED — $*" >&2; exit 1; }
not_executed() { echo "iso-qemu: NOT EXECUTED: $*"; exit 0; }

iso="${LEGE_ISO:-}"
if [ -z "$iso" ]; then
    dir="${LEGEOS_BUILD_DIR:-/var/tmp/legeos-build/@build}"
    iso="$(ls -t "$dir"/legeos-*.iso 2>/dev/null | head -1 || true)"
fi
[ -n "$iso" ] && [ -f "$iso" ] || not_executed "no ISO (build one with 'just iso', or set LEGE_ISO)"
command -v qemu-system-x86_64 >/dev/null 2>&1 || not_executed "qemu-system-x86_64 is not installed"
code="${LEGE_OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.secboot.fd}"
vars="${LEGE_OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.ms.fd}"
[ -f "$code" ] && [ -f "$vars" ] || not_executed "no Secure-Boot OVMF (ovmf package: OVMF_CODE_4M.secboot.fd + OVMF_VARS_4M.ms.fd)"

work="$(mktemp -d /tmp/legeos-iso-qemu.XXXXXX)"
trap 'rm -rf -- "$work"' EXIT
cp "$vars" "$work/vars.fd"   # the MS-keys-enrolled variable store; writable copy
serial="$work/serial.log"
accel=tcg; [ -w /dev/kvm ] && accel=kvm
timeout="${LEGE_ISO_TIMEOUT:-420}"
echo "iso-qemu: booting $iso with Secure Boot ($accel, ${timeout}s)"
timeout "$timeout" qemu-system-x86_64 \
    -machine q35,smm=on,accel="$accel" -cpu max -m 4096 -smp 2 \
    -global driver=cfi.pflash01,property=secure,value=on \
    -drive "if=pflash,format=raw,readonly=on,file=$code" \
    -drive "if=pflash,format=raw,file=$work/vars.fd" \
    -drive "file=$iso,format=raw,media=cdrom,if=ide" \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
    -serial "file:$serial" -display none >/dev/null 2>&1 || true

[ -s "$serial" ] || fail "no serial output at all from $iso"
grep -qi 'Security Violation\|Verification failed\|MokManager' "$serial" \
    && fail "the firmware refused something in the chain (see: $(grep -i 'Security Violation\|Verification failed\|MokManager' "$serial" | head -1))"
grep -q 'boot=casper' "$serial" || fail "the kernel never started (no casper command line on the console)"
grep -qi 'Secure boot enabled' "$serial" || fail "the kernel did not report Secure Boot enabled"
grep -q 'Reached target .*Graphical Interface\|Started .*greetd\|legeos-live login:' "$serial" \
    || fail "userspace did not reach the session (last lines: $(tail -3 "$serial" | tr '\n' '|'))"
echo "iso-qemu: OK — shim → GRUB → kernel with Secure Boot on, live userspace reached"
