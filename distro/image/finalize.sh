#!/usr/bin/env bash
# Offline native C3 finalization. Only an explicitly named, unused regular disk
# image is accepted; all loop devices/mounts belong to this invocation.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
private=0
read_only=0
while (($#)); do
    case "$1" in
        --in-private) private=1; shift ;;
        --read-only) read_only=1; shift ;;
        --help) printf 'usage: sudo bash finalize.sh [--read-only] IMAGE.raw\n'; exit 0 ;;
        --*) printf 'finalize: unknown argument\n' >&2; exit 1 ;;
        *) [[ $# = 1 ]] || { printf 'finalize: exactly one image is required\n' >&2; exit 1; }; image="$1"; shift ;;
    esac
done
[[ ${EUID} = 0 ]] || { printf 'finalize: offline loop/mount authority requires root\n' >&2; exit 1; }
[[ -n ${image:-} && -f "$image" && ! -L "$image" ]] || { printf 'finalize: name an unused regular, non-symlink disk image\n' >&2; exit 1; }
image="$(realpath -- "$image")"
if [[ "$private" = 0 ]]; then
    flags=()
    [[ "$read_only" = 0 ]] || flags+=(--read-only)
    exec unshare --mount --propagation private -- bash "$0" --in-private "${flags[@]}" "$image"
fi
# Refuse signatures before changing the disk, not after invalidating them.
python3 - "$image" <<'PY'
from pathlib import Path
import sys
image = Path(sys.argv[1])
checksums = list(image.parent.glob('*SHA256SUMS'))
for path in [image, *checksums]:
    if path.is_symlink():
        raise SystemExit('finalize: checksum/image symlinks are refused')
    for suffix in ('.asc', '.gpg', '.sig', '.p7s'):
        if Path(str(path) + suffix).exists():
            raise SystemExit('finalize: signed image/checksum needs an explicit valid re-signing path; refused before mutation')
PY
[[ -z "$(losetup -j "$image")" ]] || { printf 'finalize: image already has a loop device; refusing another owner\n' >&2; exit 1; }
work="$(mktemp -d /tmp/templaros-finalize.XXXXXX)"
loop=""
mounts=()
cleanup() {
    local status=$? index
    for ((index=${#mounts[@]}-1; index>=0; index--)); do umount -- "${mounts[index]}" || status=1; done
    [[ -z "$loop" ]] || losetup -d -- "$loop" || status=1
    rmdir -- "$work/root" "$work/top" "$work" 2>/dev/null || true
    return "$status"
}
trap cleanup EXIT
loop="$(losetup --find --show --partscan -- "$image")"
udevadm settle
mapfile -t partitions < <(lsblk -nrpo PATH,PARTTYPE "$loop" | while read -r path type; do
    case "$type" in
        c12a7328-f81f-11d2-ba4b-00a0c93ec93b) printf 'esp=%s\n' "$path" ;;
        4f68bce3-e8cd-4db1-96e7-fbcaf984b709) printf 'root=%s\n' "$path" ;;
    esac
done)
root_part=""
esp_part=""
for partition in "${partitions[@]}"; do
    case "$partition" in
        root=*) [[ -z "$root_part" ]] || exit 1; root_part="${partition#root=}" ;;
        esp=*) [[ -z "$esp_part" ]] || exit 1; esp_part="${partition#esp=}" ;;
    esac
done
[[ -n "$root_part" && -n "$esp_part" ]] || { printf 'finalize: native x86-64 root/ESP roles missing or duplicated\n' >&2; exit 1; }
[[ "$(blkid -s TYPE -o value "$root_part")" = btrfs && "$(blkid -s TYPE -o value "$esp_part")" = vfat ]] || exit 1
root_uuid="$(blkid -s UUID -o value "$root_part")"
esp_uuid="$(blkid -s UUID -o value "$esp_part")"
esp_partuuid="$(blkid -s PARTUUID -o value "$esp_part")"
mkdir "$work/top" "$work/root"
mount -o ro,subvolid=5 "$root_part" "$work/top"
mounts+=("$work/top")
[[ -f "$work/top/@/usr/share/templaros/layout.toml" && -f "$work/top/@/usr/share/templaros/state-compat.toml" ]] || { printf 'finalize: native @ root/reader contract missing\n' >&2; exit 1; }
btrfs subvolume show "$work/top/@" >/dev/null
[[ "$(btrfs property get -ts "$work/top/@" ro)" = ro=false ]] || { printf 'finalize: already sealed root requires an owned migration, not broad unsealing\n' >&2; exit 1; }
umount "$work/top"
mounts=()
mount -o rw,subvol=@ "$root_part" "$work/root"
mounts+=("$work/root")
root="$work/root"
layout="$root/usr/share/templaros/layout.toml"
mapfile -t paths < <(python3 - "$layout" <<'PY'
import importlib.machinery,importlib.util,sys,tomllib
from pathlib import Path
helper=Path(sys.argv[1]).parents[2]/'libexec/templaros-deployment-layout'
loader=importlib.machinery.SourceFileLoader('templaros_deployment_layout',str(helper))
spec=importlib.util.spec_from_loader(loader.name,loader)
if spec is None or spec.loader is None:
    raise SystemExit('finalize: installed deployment layout helper missing')
module=importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
v=tomllib.load(open(sys.argv[1],'rb'))
module.bindings(v,True)
print(v['state']['store_mount'])
print(v['deployment']['mount'])
print(v['esp']['mount'])
print(v['deployment']['subvolume_template'].split('{id}')[0].rstrip('/'))
PY
)
[[ ${#paths[@]} = 4 ]] || { printf 'finalize: native layout preflight failed before mount-path writes\n' >&2; exit 1; }
state_mount="${paths[0]}"
deployment_mount="${paths[1]}"
esp_mount="${paths[2]}"
deployment_subvolume="${paths[3]}"
for path in "$state_mount" "$deployment_mount" "$esp_mount"; do mkdir -p "$root$path"; done
mount -o rw,subvol=@var "$root_part" "$root$state_mount"
mounts+=("$root$state_mount")
mount -o rw,subvol="$deployment_subvolume" "$root_part" "$root$deployment_mount"
mounts+=("$root$deployment_mount")
mount "$esp_part" "$root$esp_mount"
mounts+=("$root$esp_mount")
mkdir -p "$root/etc/templaros"
printf 'root_uuid = "%s"\nesp_uuid = "%s"\nesp_partuuid = "%s"\nread_only = %s\n' \
    "$root_uuid" "$esp_uuid" "$esp_partuuid" "$([[ "$read_only" = 1 ]] && printf true || printf false)" > "$root/etc/templaros/install.toml"
flags=()
[[ "$read_only" = 0 ]] || flags+=(--read-only)
python3 "$root/usr/libexec/templaros-deployment-layout" --root "$root" --layout "$layout" "${flags[@]}"
# UUID every Btrfs/ESP role and replace any original mkosi boot default with the
# same immutable kernel/initramfs pair and identity hostd uses for future roots.
python3 - "$root" "$root_uuid" "$esp_partuuid" "$esp_mount" "$state_mount" "$read_only" <<'PY'
from pathlib import Path
import json,os,re,sys,tomllib
root=Path(sys.argv[1]); uuid,esp_uuid,esp_mount,state_mount=sys.argv[2:6]; readonly=sys.argv[6]=='1'
fstab=root/'etc/fstab'; rows=[]; found=False
for line in fstab.read_text().splitlines():
    fields=line.split()
    if fields and not line.lstrip().startswith('#') and len(fields)>=4:
        if fields[2]=='btrfs': fields[0]='UUID='+uuid
        if fields[1]==esp_mount: fields[0]='PARTUUID='+esp_uuid; found=True
        line='\t'.join(fields)
    rows.append(line)
if not found: rows.append('PARTUUID='+esp_uuid+'\t'+esp_mount+'\tvfat\tumask=0077\t0 2')
fstab.write_text('\n'.join(rows)+'\n')
id='image-'+uuid[:8]; esp=root/esp_mount.lstrip('/'); directory=esp/'EFI/templaros'/id; directory.mkdir(parents=True,exist_ok=True)
boot=root/'boot'; versions=[p.name.removeprefix('vmlinuz-') for p in boot.glob('vmlinuz-*') if (boot/('initrd.img-'+p.name.removeprefix('vmlinuz-'))).is_file()]
if not versions: raise SystemExit('finalize: no Canonical kernel/initramfs pair')
key=lambda version: tuple((1,int(part)) if part.isdigit() else (0,part) for part in re.split(r'(\d+)',version))
version=max(versions,key=key)
import shutil
shutil.copyfile(boot/('vmlinuz-'+version),directory/'vmlinuz'); shutil.copyfile(boot/('initrd.img-'+version),directory/'initrd.img')
mode='ro' if readonly else 'rw'
entry=esp/'loader/entries'/('templaros-'+id+'.conf'); entry.parent.mkdir(parents=True,exist_ok=True)
entry.write_text('title TemplarOS ('+id+')\nlinux /EFI/templaros/'+id+'/vmlinuz\ninitrd /EFI/templaros/'+id+'/initrd.img\noptions root=UUID='+uuid+' rootflags=subvol=@ '+mode+' templaros.deployment='+id+'\n')
loader=esp/'loader/loader.conf'; loader.parent.mkdir(parents=True,exist_ok=True)
lines=[line for line in loader.read_text().splitlines() if not line.startswith('default ')] if loader.exists() else []
loader.write_text('\n'.join(['default '+entry.name,*lines])+'\n')
hostd=root/state_mount.lstrip('/')/'var/lib/templaros/hostd'; hostd.mkdir(parents=True,exist_ok=True)
deployment={'id':id,'subvolume':'@','esp_dir':'/EFI/templaros/'+id,'kernel':None,'nvidia_modules':None,'known_good':True,'read_only':readonly}
(hostd/'deployments.json').write_text(json.dumps({'schema':1,'deployments':[deployment],'current':id,'previous':None,'known_good':id})+'\n')
os.chmod(hostd/'deployments.json',0o600)
PY
sync -f "$root"
sync -f "$root$state_mount"
sync -f "$root$esp_mount"
if [[ "$read_only" = 1 ]]; then btrfs property set -ts "$root" ro true; fi
# Finish owned mounts before hashing the exact disk bytes.
for ((index=${#mounts[@]}-1; index>=0; index--)); do umount -- "${mounts[index]}"; done
mounts=()
losetup -d -- "$loop"
loop=""
python3 - "$image" <<'PY'
from pathlib import Path
import hashlib,os,re,stat,sys,tempfile
image=Path(sys.argv[1]); digest=hashlib.file_digest(image.open('rb'),'sha256').hexdigest()
checksums=list(image.parent.glob('*SHA256SUMS'))
if not checksums: checksums=[image.parent/'SHA256SUMS']
for path in checksums:
    previous=path.stat() if path.exists() else image.stat()
    lines=[]; updated=False
    for line in path.read_text().splitlines() if path.exists() else []:
        match=re.fullmatch(r'[0-9a-fA-F]{64}\s+\*?(.+)',line)
        if match and Path(match.group(1)).name==image.name:
            lines.append(digest+'  '+match.group(1)); updated=True
        else: lines.append(line)
    if not updated: lines.append(digest+'  '+image.name)
    fd,name=tempfile.mkstemp(prefix='.templaros-checksum-',dir=path.parent)
    try:
        os.fchmod(fd,0o644); os.fchown(fd,previous.st_uid,previous.st_gid)
        with os.fdopen(fd,'w') as stream: stream.write('\n'.join(lines)+'\n'); stream.flush(); os.fsync(stream.fileno())
        os.replace(name,path)
    finally:
        if os.path.exists(name): os.unlink(name)
print('finalize: native disk identities/state/boot pair complete; checksum refreshed. Boot/account recovery proofs remain mandatory.')
PY
