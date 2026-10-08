#!/usr/bin/python3
"""Prepare an OFFLINE native root's declared state binds and deployment mounts.

The root remains writable while the installer runs. This helper does not remount,
seal, format, choose boot defaults, alter NSS/PAM, or touch user Work.
"""
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib

SCHEMA = 1
PACKAGE_PATHS = ("/var/lib/dpkg", "/var/lib/apt", "/var/cache/apt")


def safe_absolute(value):
    return isinstance(value, str) and value.startswith("/") and value != "/" and not any(ord(character) < 32 or character.isspace() for character in value) and all(
        part not in (".", "..") for part in value.split("/"))


def below(root, value):
    if not safe_absolute(value):
        raise RuntimeError("unsafe layout path")
    path = root / value.lstrip("/")
    # Existing symlinks must not redirect privileged copy/removal out of the target.
    for parent in (path, *path.parents):
        if parent == root:
            break
        if parent.is_symlink():
            raise RuntimeError("state path traverses a symlink")
    return path


def atomic(path, data, mode=0o644):
    fd, name = tempfile.mkstemp(prefix=".templaros-layout-", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def bindings(layout, read_only):
    state = layout["state"]
    if type(state["schema"]) is not int or state["schema"] != SCHEMA:
        raise RuntimeError("unsupported state layout; refusing downgrade")
    store = state["store_mount"]
    if not safe_absolute(store) or state["subvolume"] not in [path.lstrip("/") for path in layout["root"]["subvolumes"]]:
        raise RuntimeError("state store is not a declared subvolume")
    deployment = layout["deployment"]
    template = deployment["subvolume_template"]
    prefix = template.removesuffix("/{id}")
    if (template.count("{id}") != 1 or not template.endswith("/{id}") or
            not re.fullmatch(r"@[A-Za-z0-9_-]+(?:/[A-Za-z0-9_-]+)*", prefix) or
            "/" + prefix not in layout["root"]["subvolumes"]):
        raise RuntimeError("candidate namespace must name a declared root subvolume")
    if not all(safe_absolute(path) for path in (deployment["mount"], deployment["bootstrap_mount"], layout["esp"]["mount"])):
        raise RuntimeError("unsafe native deployment mount")
    rows = []
    for entry in state["path"]:
        target, source = entry["target"], entry["source"]
        if entry.get("read_only_only", False) and not read_only:
            continue
        if not safe_absolute(target) or not safe_absolute(source) or entry["tier"] != "state" or not entry["owner"]:
            raise RuntimeError("invalid state path declaration")
        if entry.get("file", False):
            raise RuntimeError("atomic system-file writers require root-paired ownership, not per-file binds")
        if any(path == target or path.startswith(target + "/") or target.startswith(path + "/") for path in PACKAGE_PATHS):
            raise RuntimeError("package database/cache must remain root-paired System, never a state bind")
        if target.startswith(("/home/", "/data/")) or target in ("/home", "/data", "/etc", "/var", store):
            raise RuntimeError("blanket or Work state binds are forbidden")
        rows.append((store + source, target))
    for _, target in rows:
        if sum(target == other or target.startswith(other + "/") for _, other in rows) != 1:
            raise RuntimeError("state binds must be distinct non-overlapping domains")
    return rows


def prepare(root, layout, read_only):
    bindings(layout, read_only)
    fstab_path = root / "etc/fstab"
    text = fstab_path.read_text() if fstab_path.exists() else ""
    if any(len(fields) >= 2 and fields[1] == "/var" for line in text.splitlines()
           if not line.lstrip().startswith("#") for fields in [line.split()]):
        raise RuntimeError("legacy blanket /var mount requires reviewed offline migration before any state writes")
    for line in text.splitlines():
        fields = line.split()
        if not line.lstrip().startswith("#") and len(fields) >= 4 and fields[1] == "/etc/templaros/hardware":
            if fields[2] != "none" or "bind" not in fields[3].split(","):
                raise RuntimeError("legacy hardware mount requires reviewed offline migration before state writes")
    store = below(root, layout["state"]["store_mount"])
    store.mkdir(parents=True, exist_ok=True)
    marker = store / ".templaros-state-schema.json"
    if marker.exists():
        previous = json.loads(marker.read_text())
        if type(previous.get("schema")) is not int or previous["schema"] != SCHEMA:
            raise RuntimeError("unknown shared state schema; refusing downgrade")
    else:
        atomic(marker, json.dumps({"schema": SCHEMA}).encode(), 0o600)
    # Preserve only user GPU policy, never a persisted lock for another root's
    # package set. The lock stays canonical root-paired System metadata.
    legacy_mode = below(root, "/etc/templaros/hardware/gpu-mode")
    target_mode = below(root, "/var/lib/templaros/hardware/gpu-mode")
    stored_mode = below(root, layout["state"]["store_mount"] + "/var/lib/templaros/hardware/gpu-mode")
    if not target_mode.exists() and not stored_mode.exists() and legacy_mode.exists():
        if not stat.S_ISREG(legacy_mode.lstat().st_mode):
            raise RuntimeError("legacy GPU policy is not a regular file")
        target_mode.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(legacy_mode, target_mode)
    for source_name, target_name in bindings(layout, read_only):
        source, target = below(root, source_name), below(root, target_name)
        source.parent.mkdir(parents=True, exist_ok=True)
        target.parent.mkdir(parents=True, exist_ok=True)
        if os.path.ismount(target):
            # Resume sees the already-mounted authoritative store. Never copy it
            # into itself or replace another mounted filesystem.
            if os.stat(target).st_dev == os.stat(source).st_dev and os.stat(target).st_ino == os.stat(source).st_ino:
                continue
            raise RuntimeError("state target is unexpectedly mounted")
        source_existed = source.exists()
        if not source_existed:
            source.mkdir(mode=0o755)
        if target.exists():
            if not target.is_dir():
                raise RuntimeError("state directory declaration names a non-directory")
            if not source_existed or any(target.iterdir()):
                subprocess.run(["cp", "--archive", "--reflink=auto", str(target) + "/.", str(source)], check=True)
            # cp success precedes removal. The package root then contains only a
            # mountpoint, not an obsolete second copy of mutable state.
            subprocess.run(["sync", "-f", str(source)], check=True)
            shutil.rmtree(target)
        target.mkdir(mode=0o755)
    machine_id = below(root, "/etc/machine-id")
    value = machine_id.read_text().strip() if machine_id.exists() else ""
    if not value or value == "uninitialized":
        atomic(machine_id, (os.urandom(16).hex() + "\n").encode(), 0o444)
    elif not re.fullmatch(r"[0-9a-f]{32}", value):
        raise RuntimeError("malformed machine identity; refusing replacement")
    roots = below(root, "/var/lib/templaros/roots")
    roots.mkdir(parents=True, exist_ok=True)
    roots.chmod(0o700)
    below(root, layout["deployment"]["bootstrap_mount"]).mkdir(parents=True, exist_ok=True)
    below(root, layout["deployment"]["mount"]).mkdir(parents=True, exist_ok=True)
    # This is the persistent store location even before its bind is mounted.
    accounts_store = below(root, layout["state"]["store_mount"] + "/var/lib/templaros/hostd/accounts")
    accounts_store.mkdir(parents=True, exist_ok=True)
    accounts_store.chmod(0o700)
    etc = below(root, "/etc/templaros")
    etc.mkdir(parents=True, exist_ok=True)
    install_path = etc / "install.toml"
    install = tomllib.loads(install_path.read_text()) if install_path.exists() else {}
    install["read_only"] = read_only
    lines = []
    for key, value in install.items():
        if not re.fullmatch(r"[a-z_]+", key) or not isinstance(value, (str, bool)):
            raise RuntimeError("unsupported install identity field")
        lines.append(key + " = " + (str(value).lower() if isinstance(value, bool) else json.dumps(value)))
    atomic(install_path, ("\n".join(lines) + "\n").encode())
    output = []
    uuid = install.get("root_uuid")
    device = "UUID=" + uuid if uuid else "LABEL=" + layout["root"]["label"]
    for line in text.splitlines():
        fields = line.split()
        if line.lstrip().startswith("#") or len(fields) < 4:
            output.append(line)
            continue
        if fields[1] == "/var":
            raise RuntimeError("legacy blanket /var mount requires reviewed offline migration before candidate updates")
        if fields[1] == "/etc/templaros/hardware":
            if fields[2] != "none" or "bind" not in fields[3].split(","):
                raise RuntimeError("legacy hardware mount requires reviewed offline migration")
            continue
        if fields[1] == "/":
            options = [option for option in fields[3].split(",") if option not in ("ro", "rw")]
            options.append("ro" if read_only else "rw")
            fields[3] = ",".join(options)
        output.append("\t".join(fields))
    existing = {line.split()[1] for line in output if line.strip() and not line.lstrip().startswith("#") and len(line.split()) > 1}
    declared = [("/", "@"), ("/home", "@home"), (layout["state"]["store_mount"], layout["state"]["subvolume"]),
        ("/.snapshots", "@snapshots"), (layout["deployment"]["mount"], layout["deployment"]["subvolume_template"].split("{id}")[0].rstrip("/")),
        (layout["deployment"]["bootstrap_mount"], "@")]
    for target, _ in declared:
        if target != "/":
            below(root, target).mkdir(parents=True, exist_ok=True)
    for target, subvolume in declared:
        if target not in existing:
            mode = "ro" if target == layout["deployment"]["bootstrap_mount"] or (target == "/" and read_only) else "rw"
            output.append(device + "\t" + target + "\tbtrfs\t" + mode + ",subvol=" + subvolume + ",compress=zstd:1\t0 " + ("1" if target == "/" else "0"))
    for source, target in bindings(layout, read_only):
        if target not in existing:
            output.append(source + "\t" + target + "\tnone\tbind,x-systemd.requires-mounts-for=" + source + "\t0 0")
    if "/tmp" not in existing:
        output.append("tmpfs\t/tmp\ttmpfs\tmode=1777,nosuid,nodev\t0 0")
    atomic(fstab_path, ("\n".join(output) + "\n").encode())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path)
    parser.add_argument("--layout", type=Path, required=True)
    parser.add_argument("--read-only", action="store_true")
    parser.add_argument("--bindings", action="store_true")
    parser.add_argument("--image-build", action="store_true")
    args = parser.parse_args()
    layout = tomllib.loads(args.layout.read_text())
    if args.bindings:
        for source, target in bindings(layout, args.read_only):
            print(source + "\t" + target)
        return
    if args.root is None or os.geteuid() != 0:
        raise RuntimeError("state preparation requires an explicitly named offline root and root authority")
    root = args.root.resolve(strict=True)
    if root == Path("/") and not args.image_build:
        raise RuntimeError("refusing to prepare the running root")
    prepare(root, layout, args.read_only)


if __name__ == "__main__":
    main()
